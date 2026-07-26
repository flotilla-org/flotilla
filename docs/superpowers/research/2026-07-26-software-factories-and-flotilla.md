# Software Factories, Harness Engineering, and Flotilla

**Date:** 2026-07-26
**Context:** Issue [#1028](https://github.com/flotilla-org/flotilla/issues/1028)
asked what Dex Horthy's “Why Software Factories Fail” argument contributes to
Flotilla's agents, briefs, and context-economy direction, and where it agrees or
disagrees with Ryan Lopopolo's harness-engineering field guide.

---

## Executive summary

Dex Horthy and Ryan Lopopolo agree on more than their rhetoric suggests. Both
reject token volume, generated code, tests, and pull-request count as sufficient
measures of success. Both put product intent, architecture, proof, human
judgment, and the long-term cost of change back into the acceptance boundary.
Both favor work that stays observable and independently useful while it is in
flight.

Their real disagreement is about the present reliability ceiling. Dex argues
that maintainability has no fast, reliable oracle, so a lights-off factory
cannot compensate for current model limitations with more harnesses, review
agents, or test loops. He therefore keeps humans in product design, system
architecture, program design, vertical-slice planning, and code review. Ryan's
field guide is more optimistic that organizational judgment can accumulate in
the harness until some users can accept domain outcomes without reviewing the
implementation. It nevertheless records long-term architectural coherence as
an open problem and reserves difficult interfaces for sustained human judgment.
The useful synthesis is:

> Use the harness to move repeated judgment into the environment, but do not
> pretend that today's automated proof establishes maintainability. Spend human
> attention where future regret is decided, and make that attention teach the
> next run.

Flotilla already embodies most of the operating model:

- issue bodies and Briefs carry durable intent;
- grills, wayfinder tickets, and prototypes front-load product and architecture
  decisions;
- adapted skills turn specs into tracer-bullet tickets;
- wayfinder maps/specs retain the multi-ticket destination while Convoys launch
  individual slices and can coordinate specialized crews;
- pull-request delivery and settlement remain explicit, with a review surface
  but no default human-approval gate;
- the token-burn and trace-analysis direction turns repeated failure into
  environment changes.

The candidate gap worth testing is **program design**: an artifact between
architecture and implementation that sketches call-stack changes, file-tree
changes, and the key types or interfaces. Flotilla's current adapted spec skill
records implementation decisions but deliberately excludes paths and snippets
outside a prototype exception; it does not routinely request call-stack trees
or file-tree diffs. Its ticket skill starts one step later with vertical slices.
That leaves the seam Dex identifies, but this research found no Flotilla failure
that proves the missing artifact caused rework.

The recommendation is to experiment with a small, optional **program-shape
artifact** in the adapted skill layer for high-regret work. Do not add Dex's four
phases to `WorkflowTemplate`, the resource model, or the convoy state machine.
ADR 0008 says workflow semantics are harvested from repeated practice and
frozen as programs, not designed into a declarative language in advance. If the
artifact repeatedly pays for itself, a workflow program or Brief-template
variant can invoke it without changing Flotilla's control-plane model.

## Sources and retrieval

The X status in #1028 points to an X Article rather than a conventional thread.
The direct X page remained unreadable without a connected browser. HumanLayer
also maintains the article as a first-party repository document, which is the
reviewed source used here:

- [Dex's original status and X Article](https://x.com/dexhorthy/status/2081058573556306030)
- [“Why Software Factories Fail” at the reviewed HumanLayer revision](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md)
- [Ryan Lopopolo's harness-engineering field guide at the reviewed revision](https://github.com/lopopolo/harness-engineering/tree/226c8d35fb6ea3ed55467753dba6dea2b5fd5778)

The HumanLayer document combines the argument's two parts and includes its
examples and linked sources.

## Dex's argument

### The claim is narrower than “harnesses do not help”

Dex is not arguing against tools, tests, context engineering, automated review,
or feedback loops. He says they raise the floor by catching defects, but cannot
move the ceiling imposed by what models were trained and rewarded to do.
Short-horizon benchmarks reward making tests pass and not regressing known
behavior; they do not penalize a change whose architectural cost appears months
later. He therefore treats claims about current models' ability to preserve
maintainability as unproved. [The article explicitly separates fast test
feedback from the long cost function of bad architecture and calls
maintainability's oracle an unsolved problem.](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md#L347-L399)

His production claim is correspondingly scoped: lights-off operation is unsafe
for hard work in complex, long-lived codebases. One-off fixes, small scripts,
and low-consequence projects remain suitable for one-shot execution. He reports
that HumanLayer abandoned a lights-off experiment after several months because
humans eventually had to debug a codebase they had stopped reading.
[The one-shot examples are explicit in the workflow's scope.](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md#L418-L435)
[(The failed experiment is described here.)](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md#L209-L228)

### Front-load the decisions that are expensive to reverse

Dex's lights-on workflow has four phases:

1. **Product requirements** — the problem in the user's language and the
   observable meaning of success; use a mockup when prose is a low-bandwidth
   substitute.
2. **System architecture** — services, endpoints, schemas, queues, stores, and
   transformations.
3. **Program design** — the shape of code: call-stack changes, file-tree
   changes, types, method signatures, and important interfaces.
4. **Vertical slices** — touchable, testable paths through the system rather
   than migrations, services, API, and UI built as separate horizontal batches.

The model drafts these artifacts, while people make or approve the
high-leverage decisions. Review can happen before implementation with the same
person who would otherwise discover the disagreement in the pull request.
[The complete four-phase description and review model are in the source.](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md#L403-L536)

Program design is the novel part. Architecture diagrams can agree on components
while leaving the implementation's dependency direction, module seams, and
call-stack shape implicit. A short pseudocode or diff artifact makes those
decisions discussable while they are cheap to change. Dex's suggested artifact
is intentionally light:

- a call-stack tree, using diff syntax when the change is the point;
- a file-tree diff with the role of each new or changed file;
- the key types and method signatures.

[The program-design examples are concrete rather than a request for a large
design document.](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md#L481-L536)

### Slice by uncertainty and feedback, not only by size

Dex says roughly 40% of tasks remain one-shot candidates, medium work can use
one combined product/system plan, and large work can use all four phases. More
important than those percentages is the feedback shape: implement one to three
vertical slices, exercise the real behavior, and re-steer before a multi-thousand
line result accumulates. [His vertical-slice example and sizing heuristic are
here.](https://github.com/humanlayer/advanced-context-engineering-for-coding-agents/blob/418c1dbaa9b71592bc58c44074cacb85a3092c7f/wsff.md#L538-L587)

Raw size should not become Flotilla's admission rule. A large mechanical
migration with a complete population and executable invariant may need little
human design. A small change to a central interface may carry enormous future
regret. The useful trigger is **ambiguity × consequence × reversibility**, not
lines, tokens, or the issue's apparent size.

## Comparison with Lopopolo's harness-engineering field guide

| Question | Dex Horthy | Ryan Lopopolo | Synthesis for Flotilla |
|---|---|---|---|
| What is the unit of success? | A useful, maintainable change, not generated code or a passing benchmark. | A valuable, proved outcome; tokens, lines, checks, and PRs are inputs rather than outcomes. | A map/spec carries the multi-ticket outcome; a Convoy's phase is testimony about its launched work, never proof of value. |
| Where does quality come from? | Human judgment at product, architecture, program-design, slice, and code-review boundaries. | Institutional context, examples, tools, types, checks, proof, and consequential human authority accumulated in the environment. | Preserve human judgment, then promote recurring parts into the Hull, skills, repository, and tests. |
| Can tests/review agents replace code review? | No for maintainability today; there is no fast reliable oracle for future change cost. | Domain proof can let some users accept an outcome without reviewing implementation, but long-term coherence remains open and contextual judgment remains in review. | Claim-matched proof can compress review; it must not be misrepresented as proof of maintainability. |
| How should a large job run? | Front-load alignment, then implement and review one to three vertical slices at a time. | One primary trajectory owns the whole outcome, while dependency-aware independently provable pieces and subagents support it. | A wayfinder map/spec can own the destination while each ticket Convoy launches one slice; a future workflow program may coordinate several Vessels inside one Convoy. |
| How does the system improve? | Learn model constraints and optimize the process within them. | Capture trajectories and outcomes, recover the failure class, and promote stable lessons into their earliest durable owner. | This is #838/#940: traces should produce environment, skill, tool, or architecture changes, not permanent transcript stuffing. |
| How much context? | Front-load decision-rich artifacts and keep work touchable; the broader context-engineering lineage favors intentional, bounded context. | Keep a large navigable store and a small active working set, retrieving policy at the latest reliable point. | Keep the full immutable Brief snapshot for honesty, with short positive routing and just-in-time skills around it. |

Ryan's “one primary trajectory” retains responsibility for decomposition,
integration, proof, and closure; supporting subagents provide evidence rather
than dividing that accountability. [The whole-job thesis makes that ownership
explicit.](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/docs/whole-job/README.md#L141-L164)

Ryan's guide makes two concessions that materially narrow the apparent
disagreement.

First, it calls multi-year architectural coherence an open question even after
an internally successful five-month, roughly 1,500-PR example. It says difficult
interface refactors still need sustained human judgment and that reliable
foresight across future changes is an open capability.
[Those limits are explicit in the durable-systems thesis.](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/docs/durable-systems/README.md#L14-L34)
[(Its architectural-program comparison is here.)](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/docs/durable-systems/README.md#L94-L164)

Second, “users who never review implementation” describes a domain boundary,
not proof that nobody with engineering responsibility ever evaluates design or
code. Ryan still makes product intent, nonfunctional requirements, architecture,
proof, review, and lifetime ownership part of the harness.
[The field guide's compact statement of that boundary is here.](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/README.md#L10-L57)

Dex supplies the caution Ryan's program needs: known requirements and executable
invariants can move into tooling, but qualitative maintainability judgments
cannot be declared automated merely because many proxy checks exist. Ryan
supplies the compounding loop Dex's program needs: a human correction should not
remain an eternal manual ritual when it can become a type, API, tool, test,
skill, or architecture rule. [Ryan's feedback thesis explicitly routes each
class of lesson to the smallest durable owner.](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/docs/feedback/README.md#L59-L128)

## What Flotilla already does

### Durable intent, small operating layer

ADR 0015 requires a source-qualified, timestamped issue snapshot in every
issue-started Brief. This is deliberately fuller than an aggressively minimal
prompt because a contained crew may lack forge access and because the Brief is
the honest script as executed. Discarding or replacing the snapshot with a
summary would violate that ruling.
[ADR 0015 explains the snapshot boundary.](../../adr/0015-intent-completion-at-admission.md#the-brief-embeds-an-issue-snapshot)

The compatible context-economy move is to separate:

- **immutable source context** — full issue snapshot and human instruction;
- **short operating context** — role, settlement gate, interaction style,
  available skills, and routes to authoritative project knowledge;
- **just-in-time context** — code, ADRs, tools, errors, reviewers, and proof
  loaded when a decision needs them.

Issue [#960](https://github.com/flotilla-org/flotilla/issues/960) has already
made the operating layer overridable and workflow-specific. The built-in Brief
is a short skeleton with blocks for crew, operating instructions, delivery, and
assignment; repository, project, and installation overrides can replace those
blocks. [The built-in template is the current
evidence.](../../../crates/flotilla-core/src/agent_adapter/templates/crew.md)
[The resolver orders installation, project, and repository
overrides](../../../crates/flotilla-core/src/agent_adapter.rs#L77-L104), and
[the renderer layers them with
Minijinja](../../../crates/flotilla-core/src/agent_adapter.rs#L186-L247).

### Product and architecture alignment

Flotilla's grills and wayfinder maps already perform much of Dex's product and
system-architecture work:

- a grill resolves ambiguous human intent and records the ruling;
- a prototype makes an uncertain interaction or state model concrete before
  production implementation;
- ADRs carry architectural decisions forward;
- issue bodies become dispatchable contracts.

The compact-rail prototype in
[#973](https://github.com/flotilla-org/flotilla/issues/973) is a direct example
of Dex's mockup claim: a cheap artifact revealed that existing compact rendering
was already close to the desired form and prevented unnecessary new machinery.

### Tracer-bullet delivery

The adapted `to-tickets` skill already requires complete, demonstrable,
single-context vertical slices and explicit dependency edges. Wide refactors use
expand-contract sequencing rather than being forced into artificial slices.
This is substantially the same discipline as Dex's vertical-slice phase.
[The public upstream skill states the rule
directly.](https://github.com/mattpocock/skills/blob/ed37663cc5fbef691ddfecd080dff42f7e7e350d/skills/engineering/to-tickets/SKILL.md#L25-L40)

Wayfinder retains the multi-ticket destination in its map while this
repository's execution policy launches each dispatchable ticket as a separate
Convoy. Within one Convoy, Briefs, prompts, git, and handoffs carry the
fine-grained crew workflow.
[The issue-tracker policy defines the per-ticket dispatch.](../../agents/issue-tracker.md#wayfinding-execution)
[The roadmap establishes Convoy as the launched-work path.](../../roadmap.md#sequencing)

### Review and settlement remain real boundaries

The built-in coder Brief requires a pushed branch, a ready pull request, and
green checks before a crew may report completion. It creates a human-review
surface, but does not require human approval before that completion report.
ADR 0017 goes further: completion is testimony, not proof that work landed or
is safe to destroy; clean, pushed, and landed are separately observed
integration conditions.
[ADR 0017 preserves these planes.](../../adr/0017-convoy-completion-claims-conditions-attention.md#the-three-planes)

Flotilla therefore preserves the opportunity for lights-on review and a
human-controlled integration boundary, but does not implement Dex's full human
review gate by default. One opportunity is to make the optional review input
more decision-ready. Ryan's proof guidance recommends a compact packet
containing the outcome, material design/risk decisions, exact checks and
journeys, relevant screenshots/logs/traces, known limits, and artifact identity.
[That packet is described here.](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/docs/proof/README.md#L136-L159)
Flotilla's standard Brief demands green CI, but does not yet ask for
claim-matched behavioral evidence or known limits.

### Trace-driven improvement is already the destination

Issue [#838](https://github.com/flotilla-org/flotilla/issues/838) proposes
detecting repeated token burn across crew traces, eliminating known traps in
provisioning, replacing noisy generic tools with terse project-specific ones,
and promoting fixes that recur across projects. Its
[context-poisoning comment](https://github.com/flotilla-org/flotilla/issues/838#issuecomment-5045917417)
also recognizes that a failure narrative inside a body-is-contract Brief can
prime the behavior it warns against.

Issue [#940](https://github.com/flotilla-org/flotilla/issues/940) turns the same
observation into a steward loop over session logs, cleat recordings, forge
events, and convoy timestamps. Issue
[#955](https://github.com/flotilla-org/flotilla/issues/955) applies it to
skills: maintain local adaptations, remove duplicated work, and keep the
adaptation as a patch layer instead of a drifting fork.

Ryan's field guide independently arrives at the same sensor and promotion
model: preserve observable trajectory and outcome evidence, treat it as a lead
rather than a diagnosis, then promote stable lessons to the earliest durable
owner. [Its evidence inventory is here.](https://github.com/lopopolo/harness-engineering/blob/226c8d35fb6ea3ed55467753dba6dea2b5fd5778/docs/feedback/README.md#L14-L57)

## The program-design hypothesis

Flotilla currently moves approximately:

```text
human intent
  → grill / prototype / wayfinder ruling
  → spec or dispatchable issue
  → tracer-bullet tickets
  → crew implementation
  → pull-request review
```

Dex's proposed artifact would sit between the ruling/spec and tickets:

```diff
 human intent
   → grill / prototype / wayfinder ruling
   → spec or dispatchable issue
+  → program shape
   → tracer-bullet tickets
   → crew implementation
   → pull-request review
```

The adapted `to-spec` skill already asks for modules, interfaces, schemas, API
contracts, testing seams, and prior art. It permits prototype-derived snippets
when a state machine, schema, or type shape expresses a decision more precisely,
but otherwise excludes file paths and snippets because they go stale.
[Those instructions are explicit in the public upstream
skill.](https://github.com/mattpocock/skills/blob/ed37663cc5fbef691ddfecd080dff42f7e7e350d/skills/engineering/to-spec/SKILL.md#L13-L19)
[(The artifact rules are here.)](https://github.com/mattpocock/skills/blob/ed37663cc5fbef691ddfecd080dff42f7e7e350d/skills/engineering/to-spec/SKILL.md#L43-L65)

The narrower hypothesis is that call-stack and file-tree diffs are not produced
routinely before implementation, so some implementation-shape decisions may
first become visible in pull-request review. This research did not audit PR
rework or every neighboring design/refactor skill, so it does not establish
that this is a current Flotilla failure. The experiment below is meant to
discover whether the candidate seam pays for itself.

If the hypothesis holds, the right cut is a separate **program-shape
artifact**:

```markdown
## Program shape

### Call path
<small current→proposed call-stack tree>

### File layout
<small current→proposed file-tree diff>

### Key interfaces
<types, signatures, invariants, and error shapes whose choice carries regret>

### Proof seams
<the highest behavior seam for each slice, plus real-system evidence>

### Open judgments
<decisions a person or specialist reviewer still needs to make>
```

It is an execution aid, not a new canonical architecture document. Once the
work lands, its durable decisions belong in code, tests, types, ADRs, or the
issue resolution; stale file layouts and signatures need not become permanent
project doctrine.

## Where the idea belongs in Flotilla

### Skill and Brief layer: the first experiment

The first experiment should be a manual, instruction-level adaptation under
#955:

- add a `program-shape` skill, or a narrow optional phase used by `to-spec` /
  `to-tickets`;
- mention it in a selected workflow's Brief template or invoke it manually for
  work believed to have high ambiguity, consequence, or interface regret;
- invite author-opt-in review of the artifact before implementation without
  blocking when no reviewer is present;
- pass the accepted artifact into ticket decomposition and the implementation
  Brief.

This respects ADR 0010's Hull/Crew boundary: reusable behavior lives in skills
aboard the Hull, while the per-run artifact and instruction live in the Crew
Brief. [ADR 0010 defines that split.](../../adr/0010-crew-provisioning.md#the-hullcrew-boundary-quotable)

Brief templates only shape instructions; they do not calculate risk, select
workflow dynamically, or enforce approval. Automatic risk routing would belong
in a later orchestrating workflow program. A blocking human approval would be
an ADR 0018 Demand. Neither mechanism is part of the initial experiment.

### Workflow program: maybe, after evidence

If the sequence recurs and proves valuable, an agent-authored workflow program
can create the design/review/implementation Vessels and route the artifact
between them. The program can choose to skip the phase for low-regret work.

This is exactly ADR 0008's extraction path: dynamic practice first, recurring
shape frozen as a program later. [The ADR explicitly rejects growing
declarative workflow semantics ahead of practice.](../../adr/0008-agentic-first-orchestration.md)

### Resource model or universal `WorkflowTemplate` phase: no

Adding `ProductReview`, `ArchitectureReview`, `ProgramDesign`, or
`VerticalSliceReview` as resource phases would encode one practitioner's
workflow as control-plane ontology. It would also confuse:

- work phase with a planning artifact;
- human demand with phase completion;
- a reusable skill with orchestration state;
- an optional risk control with a universal lifecycle.

ADR 0018 already models human review as an addressed **Demand**, never as a
phase. [Its demand/regard distinction is the applicable control-plane
contract.](../../adr/0018-presentation-attention-demands-regards-projection.md#principals-and-the-two-attention-concepts)

The action meta-model direction resolved in
[#1072](https://github.com/flotilla-org/flotilla/issues/1072) does not change
this cut. That registry makes entity actions, parameters, and derived completion
forms canonical across CLI and presentation surfaces. It is not a generic
registry of development-process stages. A program-shape artifact may describe
an action interface, but creating or reviewing the artifact is workflow, not an
entity action fact.

### Plane A: nowhere

Any implementation belongs on the Convoy/Brief/skill side. Plane A is
bugfix-only, and the roadmap makes Convoy the end-to-end launch path.

## Recommended experiment

Run a bounded experiment before filing a control-plane feature:

1. Select several completed or upcoming changes with high interface regret:
   orchestration changes, shared protocol changes, state-model refactors, or
   multi-surface behavior.
2. Hold the model and coding-agent configuration constant.
3. For the treatment set, create and (when available) review the small
   program-shape artifact before ticketing or implementation.
4. Preserve both the artifact and the ordinary trajectory evidence.
5. Compare:
   - substantive review rounds and rework;
   - architecture/design findings first discovered in the PR;
   - time to accepted outcome, not only implementation time;
   - human steering and review minutes;
   - discarded code and repeated CI cycles;
   - whether the artifact's decisions survived implementation;
   - whether later changes benefited from the resulting code shape.
6. Remove or revise the adaptation if it adds ceremony without improving
   accepted outcomes.

This follows Ryan's fixed-worker evaluation boundary and Dex's theory that
planning should reduce expensive downstream review. It also supplies #838/#940
with a concrete signal: **design decisions discovered after implementation**
are a class of preventable burn.

## Decisions and follow-ups

1. **Do not reopen the control-plane model.** No new resource kind, phase, or
   declarative workflow grammar follows from this research.
2. **Test the program-shape hypothesis.** Try it in the adapted skill/Brief
   layer, separate from the durable product/architecture spec, before claiming
   a standing gap.
3. **Keep human gates risk-based and author-opt-in initially.** Do not make
   every crew wait after 100–200 lines; that would replace review cost with
   coordination cost and conflict with autonomous completion when no demand is
   present.
4. **Keep complete issue snapshots.** Improve the short positive operating
   layer and just-in-time routing rather than summarizing away the
   script-as-executed.
5. **Add claim-matched proof to the same experiment.** A program shape reduces
   design rework; a compact proof packet reduces review reconstruction. They
   attack different parts of the review bottleneck.
6. **Feed results into #955 and #838/#940.** Only promote a recurring,
   evidenced practice into a workflow program or stronger control.

The most valuable idea in Dex's article is not “add more planning.” It is to
move one particular class of judgment—the shape of the program—forward from
pull-request review to the last cheap moment before code, while leaving the
rest of Flotilla's orchestration model unchanged.
