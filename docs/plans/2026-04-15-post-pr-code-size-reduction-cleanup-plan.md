# Code size reduction cleanup plan

## Related documents

- **Original draft (archived):** `docs/plans/archive/2026-04-15-post-pr-code-size-reduction-cleanup-plan-original.md`
- **Phase 0 + 1 + Task E implementation plan:** `docs/plans/2026-04-15-code-size-reduction-phase-1-implementation.md`
- **Phase 2 (Task A) implementation plan:** `docs/plans/2026-04-15-code-size-reduction-phase-2-implementation.md`
- **Phase 3 (Tasks D, B) implementation plan:** `docs/plans/2026-04-15-code-size-reduction-phase-3-implementation.md`
- **Phase 4 (Tasks C, F) implementation plan:** `docs/plans/2026-04-15-code-size-reduction-phase-4-implementation.md`

## Goal

Reduce overall code size and review burden once the current feature work settles, without making architectural changes during the ongoing transition from the earlier gossip-oriented protocol toward the reconciler/resource model.

This cleanup should focus on:

- shared test helpers and parameterized tests
- better testing seams to reduce repetitive setup
- moving tests outward toward public interfaces and observable behavior where that reduces brittleness
- utility extraction
- small abstraction improvements that reduce boilerplate
- targeted adoption of popular boilerplate-reduction crates (`rstest`, `bon`) where they clearly reduce duplication

## Non-goals

- no architectural redesign
- no large reshaping of controller/resource boundaries
- no speculative abstractions unrelated to current duplication

## Tool decisions

Two tools are adopted upfront rather than evaluated at the end:

- **`rstest`** for parameterized tests. Decided now so fixture helpers and reconciler test compaction can target it directly from the start.
- **`bon`** for builders. Preferred over `typed-builder` for its typestate API and its ability to generate builders for functions (useful for fixture helpers so a single `#[builder]` function replaces the "named variant per permutation" pattern).

### Builder guidance for CLAUDE.md

Add a short section on when to reach for a builder:

- Derive `bon::Builder` on types with more than three fields, deep nesting, or many optional fields.
- Use `#[builder]` on fixture functions rather than enumerating named variants (`meta_with_labels`, `meta_with_owner`, etc.).
- Struct literals stay fine for flat types with one or two required fields and no optionals.
- Prefer builders at call sites where positional arguments or `..Default::default()` hurt readability.

## Phase 1: Boilerplate foundation

Derive `bon::Builder` on the types that drive most of the verbose construction in both production and tests. Doing this first means every later phase is written on top of it.

### Types to derive

- `InputMeta`
- `ControllerObjectMeta`
- `WorkflowTemplateSpec`
- `TaskDefinition`
- `ProcessDefinition`
- `PlacementPolicySpec`
- other deep spec types that surface during migration (e.g. types in `convoy.rs` if they meet the guidance above)

### Scope

- `crates/flotilla-resources/src/resource.rs`
- `crates/flotilla-resources/src/controller/mod.rs`
- `crates/flotilla-resources/src/workflow_template.rs`
- `crates/flotilla-resources/src/convoy.rs`
- follow-up migration at the worst call sites (child-resource construction inside reconcilers, deepest test fixtures)

### Acceptance criteria

- `bon` added to the workspace
- CLAUDE.md builder guidance added
- at least five inline `InputMeta { ... }` call sites converted to builder calls
- deep spec types derive `Builder` and at least one production call site per type is migrated
- the remaining phases rely on these derives instead of hand-written constructors

## Phase 2: Independent test wins

Both tasks in this phase are independent; either can be done first.

### Task E: Extract a daemon runtime git test helper

#### Goal

Shrink the `crates/flotilla-daemon/src/runtime.rs` test module and improve readability.

#### Suggested helper

`TestGitRepo` with methods like:

- `init(path)`
- `with_initial_commit()`
- `with_origin(url)`
- `head()`
- `path()`

#### Repeated setup to remove

- `git init`
- `git config user.name`
- `git config user.email`
- write README
- `git add` / `git commit`
- `git remote add origin`
- `git rev-parse HEAD`

#### Acceptance criteria

- no runtime test contains the full raw git init/config/add/commit sequence inline
- runtime tests read in terms of scenario intent rather than git plumbing

---

### Task A: Shared resource-fixture helpers

#### Goal

Remove repeated manual construction of `InputMeta`, common specs and statuses, and repeated `create` + `update_status` sequences.

With `bon` in place (Phase 1), most of this collapses to `#[builder]`-annotated fixture functions rather than a long menu of named variants.

#### Scope

Create or expand shared test helpers in:

- `crates/flotilla-resources/tests/common/mod.rs`
- `crates/flotilla-controllers/tests/common/mod.rs`

#### Helper surface (using `#[builder]` where appropriate)

- metadata fixtures: `meta(name)` with builder-style optional labels, annotations, owner references, finalizers, deletion state
- seeded-resource fixtures: `create_environment`, `create_clone`, `create_checkout`, `create_host`, `create_convoy_with_single_task`, each with a builder for status and optional fields

#### Primary file targets

- `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
- `crates/flotilla-resources/tests/controller_loop.rs`
- `crates/flotilla-resources/tests/provisioning_resources_in_memory.rs`
- daemon runtime tests (where resource fixtures are relevant)

#### Acceptance criteria

- duplicated local helper functions are removed from at least three test files
- new tests use shared helpers by default
- inline `InputMeta { ... }` blocks in test code are largely replaced by the Phase 1 builder or the fixture helpers

## Phase 3: Harness and contracts

### Task D: Controller-loop test harness

#### Goal

Remove repeated boilerplate for spawning loops, waiting for convergence, and aborting tasks.

#### Harness responsibilities

- spawn one or more controller loops
- retain join handles
- expose backend and resolvers
- provide `wait_until_*` helpers
- abort on drop or explicit shutdown

#### Relation to Task A

The harness exposes its surface in terms of Task A's fixture builders (seeding ready resources, creating owned child objects). It does not introduce a parallel helper style.

#### Scope

- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
- `crates/flotilla-resources/tests/controller_loop.rs`
- helper module under `crates/flotilla-controllers/tests/common/mod.rs`

#### Acceptance criteria

- provisioning and controller-loop tests no longer manually duplicate handle spawning/aborting patterns
- local `wait_until` implementations are consolidated into the harness

---

### Task B: Generic backend contract tests

#### Goal

Replace duplicated CRUD/watch tests with reusable `rstest`-driven contract tests parameterized over resource kind.

#### Scope

- `crates/flotilla-resources/tests/in_memory.rs`
- `crates/flotilla-resources/tests/workflow_template_in_memory.rs`
- `crates/flotilla-resources/tests/common/mod.rs`
- optional follow-up: `crates/flotilla-resources/tests/http_wire.rs` (left as a follow-up if it complicates the first pass)

#### Contracts to extract

- create / get / list roundtrip
- stale resource-version conflicts
- delete event emission
- watch-from-version replay
- watch-now semantics
- namespace isolation
- metadata roundtrip

#### Design shape

A small per-resource fixture trait or helper layer supplying:

- `meta(name)`
- `spec()`
- `updated_spec()`
- optional `status()`
- resource-specific assertions

The trait is implemented once per resource kind; the contract tests are expressed once and run against every implementation via `rstest` cases.

#### Acceptance criteria

- `in_memory.rs` and `workflow_template_in_memory.rs` share one contract harness
- duplicated create/get/list/watch boilerplate is removed between them
- adding a new resource kind requires only a new fixture impl, not new test bodies

## Phase 4: Reconciler test compaction

### Task C: Compact task workspace reconciler tests

#### Goal

Reduce line count in narrowly similar unit tests without losing edge-case coverage.

#### Primary target

- `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`

#### Secondary targets

- `crates/flotilla-controllers/tests/clone_reconciler.rs`
- `crates/flotilla-controllers/tests/environment_reconciler.rs`
- `crates/flotilla-controllers/tests/terminal_session_reconciler.rs`

#### Approach

Use `#[rstest]` with case attributes. No table-driven-first-then-maybe-rstest pass.

#### Good parameterization candidates

- placement policy strategy variants
- expected cwd variants
- host-direct vs docker provisioning outcomes
- failure propagation variants

#### Avoid parameterizing

- tests whose assertions are already compact
- tests where the parameter table would be less readable than explicit cases

#### Acceptance criteria

- `task_workspace_reconciler.rs` is materially shorter
- strategy-specific tests are expressed as cases rather than repeated setup blocks
- secondary reconciler files get the same treatment where it clearly shortens them

---

### Task F: Move happy-path reconciler tests outward

#### Goal

Test externally observable behavior more often than internal patch selection.

#### Scope

Review these files:

- `crates/flotilla-controllers/tests/clone_reconciler.rs`
- `crates/flotilla-controllers/tests/environment_reconciler.rs`
- `crates/flotilla-controllers/tests/checkout_reconciler.rs`
- `crates/flotilla-controllers/tests/terminal_session_reconciler.rs`

#### Keep unit tests only for

- validation logic
- deterministic naming rules
- failure mapping
- subtle edge cases and non-obvious branch logic

#### Move or replace happy paths with

Controller-loop tests asserting:

- resulting status phase
- child resource creation
- observable refs, IDs, and paths

Primary destination:

- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`

#### Ceiling (when to stop)

Stop moving tests when remaining reconciler unit tests cover only naming, validation, failure mapping, and subtle branch logic. A reconciler unit test that asserts on a child-resource patch for a happy-path code path is a move or delete candidate; one that asserts on failure classification or branch selection is not.

#### Acceptance criteria

- at least one happy-path unit test per reconciler is removed or merged into loop-level coverage
- remaining reconciler unit tests focus on edge cases and branch logic

## Removed from earlier drafts

These items from earlier iterations of the plan are no longer separate tasks:

- **Metadata constructors on production types** — subsumed by Phase 1 derives on `InputMeta` and `ControllerObjectMeta`.
- **Reassessing verbose nested spec construction** — subsumed by Phase 1 derives on the deep spec types.
- **Builder-macro evaluation as a final step** — decided now in favour of `bon`.

## Guardrails

- no architectural rewrites
- no changing controller/resource boundaries just to save lines
- prefer helpers over introducing new abstraction layers with their own concepts
- prefer behavior-level tests where they genuinely reduce duplication

## Success metrics

### Qualitative

- fewer local helper functions per test file
- fewer inline `InputMeta { ... }` and status-seeding blocks
- fewer tests asserting on patch variants when status or child-resource behavior would suffice
- shorter daemon runtime test module
- new tests are cheaper to write than copying an existing test and editing it

### Quantitative

Capture before/after line counts on the primary target files so the effort is legible:

- `crates/flotilla-daemon/src/runtime.rs`
- `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
- `crates/flotilla-resources/tests/in_memory.rs`
- `crates/flotilla-resources/tests/workflow_template_in_memory.rs`

## Execution order

### Phase 0: tool adoption

1. add `rstest` and `bon` to the workspace
2. add CLAUDE.md builder guidance

### Phase 1: boilerplate foundation

3. derive `bon::Builder` on metadata and deep spec types; migrate representative call sites

### Phase 2: independent test wins

4. Task E — `TestGitRepo`
5. Task A — shared resource-fixture helpers

### Phase 3: harness and contracts

6. Task D — controller-loop harness (uses Task A fixtures)
7. Task B — generic backend contract tests

### Phase 4: reconciler test compaction

8. Task C — `rstest` parameterization
9. Task F — move happy-paths outward

## Task dependency summary

- **Phase 0 and 1** are prerequisites for everything downstream.
- **Task E** is independent of Phase 1 but cheap; fine to do first or in parallel.
- **Task A** depends on Phase 1.
- **Task D** depends lightly on Task A (harness uses its fixture builders).
- **Task B** benefits from Phase 1 and Task A but can proceed in parallel with D.
- **Task C** benefits strongly from Phase 1 and Task A.
- **Task F** benefits from Tasks A, C, and D.

## Effort sizing

- **S**: small, localized change
- **M**: medium refactor with a few touch points
- **L**: broader cleanup likely best tackled in stages

| Task | Effort |
|------|--------|
| Phase 0 tool adoption | S |
| Phase 1 foundation | M |
| Task E | S |
| Task A | M |
| Task D | M |
| Task B | M |
| Task C | M |
| Task F | M to L |
