# Code size reduction cleanup plan (original, pre-brainstorm)

> **Archived.** This is the first draft of the code-size-reduction cleanup plan, preserved for diff comparison against the post-brainstorm rewrite at `docs/plans/2026-04-15-post-pr-code-size-reduction-cleanup-plan.md`. The rewrite resequenced phases, adopted `rstest` and `bon` upfront, and subsumed the original Tasks 7 and 8 plus the builder-macro evaluation (Task H) into a single Phase 1 that derives `bon::Builder` on key production types.

---

## Goal

Reduce overall code size and review burden once the current feature work settles, without making architectural changes during the ongoing transition from the earlier gossip-oriented protocol toward the reconciler/resource model.

This cleanup should focus on:

- shared test helpers and parameterized tests
- better testing seams to reduce repetitive setup
- moving tests outward toward public interfaces and observable behavior where that reduces brittleness
- utility extraction
- small abstraction improvements that reduce boilerplate
- selective use of popular boilerplate-reduction derive macros only if they still look justified after helper extraction

## Non-goals

- no architectural redesign
- no large reshaping of controller/resource boundaries
- no speculative abstractions unrelated to current duplication
- no broad builder proliferation unless narrow trials show a clear readability win

## Work items

### 1. Build a shared test fixture layer for resource objects

#### Goal

Remove repeated manual construction of:

- `InputMeta`
- common specs and statuses
- repeated `create` + `update_status` sequences

#### Scope

Create or expand shared test helpers in:

- `crates/flotilla-resources/tests/common/mod.rs`
- `crates/flotilla-controllers/tests/common/mod.rs`

#### Add helpers for

- metadata
  - `meta(name)`
  - `meta_with_labels(name, labels)`
  - `meta_with_owner(name, owner)`
  - `deleting_meta(name, finalizers)`
- common seeded resources
  - `create_with_status(...)`
  - `create_ready_environment(...)`
  - `create_ready_clone(...)`
  - `create_ready_checkout(...)`
  - `create_ready_host(...)`
  - `create_convoy_with_single_task(...)`

#### Expected payoff

Reduce repetition in:

- `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
- `crates/flotilla-resources/tests/controller_loop.rs`
- `crates/flotilla-resources/tests/provisioning_resources_in_memory.rs`
- daemon runtime tests

#### Acceptance criteria

- duplicated local helper functions are removed from at least 3 test files
- new tests use shared helpers by default

---

### 2. Build generic backend contract tests for resources

#### Goal

Replace duplicated CRUD/watch tests with reusable contract-style test helpers.

#### Scope

Refactor repeated tests in:

- `crates/flotilla-resources/tests/in_memory.rs`
- `crates/flotilla-resources/tests/workflow_template_in_memory.rs`

Potential follow-up:

- selected `crates/flotilla-resources/tests/http_wire.rs` coverage

#### Extract reusable contracts for

- create/get/list roundtrip
- stale resource version conflicts
- delete event emission
- watch from version replay
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

#### Expected payoff

Substantial reduction in repetitive test bodies and cheaper addition of new resource kinds.

#### Acceptance criteria

- `in_memory.rs` and `workflow_template_in_memory.rs` share one contract harness
- duplicated create/get/list/watch boilerplate is removed between them

---

### 3. Compact repetitive reconciler tests into parameterized cases where practical

#### Goal

Reduce line count in narrowly similar unit tests without losing edge-case coverage.

#### Scope

Primary target:

- `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`

Secondary targets:

- `crates/flotilla-controllers/tests/clone_reconciler.rs`
- `crates/flotilla-controllers/tests/environment_reconciler.rs`
- `crates/flotilla-controllers/tests/terminal_session_reconciler.rs`

#### Tactics

- prefer table-driven helper functions first
- optionally adopt `rstest` if the resulting tests are clearly shorter and still readable

#### Good candidates

- placement policy strategy variants
- expected cwd variants
- host-direct vs docker provisioning outcomes
- failure propagation variants

#### Avoid parameterizing

- tests whose assertions are already compact
- tests where the parameter table would be less readable than explicit cases

#### Acceptance criteria

- `task_workspace_reconciler.rs` is materially shorter
- strategy-specific tests are expressed as data rather than repeated setup blocks

---

### 4. Move happy-path provisioning tests outward to controller-loop behavior

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
- subtle edge cases

#### Move or replace happy paths with

Controller-loop tests asserting:

- resulting status phase
- child resource creation
- observable refs, IDs, and paths

Primary destination:

- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`

#### Expected payoff

Fewer brittle tests tied to patch variants and more confidence in actual observable behavior.

#### Acceptance criteria

- at least one happy-path unit test per reconciler is removed or merged into loop-level coverage
- remaining reconciler unit tests focus on edge cases and branch logic

---

### 5. Build a controller-loop test harness

#### Goal

Remove repeated boilerplate for:

- spawning loops
- waiting for convergence
- aborting tasks

#### Scope

Create a helper module for:

- `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
- `crates/flotilla-resources/tests/controller_loop.rs`

#### Harness responsibilities

- spawn one or more controller loops
- retain join handles
- expose backend and resolvers
- provide `wait_until_*` helpers
- abort on drop or explicit shutdown

#### Expected payoff

Cleaner behavior tests and less lifecycle noise in each test body.

#### Acceptance criteria

- provisioning and controller-loop tests no longer manually duplicate handle spawning/aborting patterns
- local `wait_until` implementations are consolidated

---

### 6. Extract git repo setup into a daemon runtime test helper

#### Goal

Shrink `crates/flotilla-daemon/src/runtime.rs` test module and improve readability.

#### Scope

Refactor repeated repo bootstrapping in runtime tests into a helper type or function.

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

#### Expected payoff

High line-count reduction in one dense file.

#### Acceptance criteria

- no runtime test contains the full raw git init/config/add/commit sequence inline
- runtime tests read in terms of scenario intent rather than git plumbing

---

### 7. Add lightweight metadata constructors to production types

#### Goal

Reduce repeated `InputMeta` and `ControllerObjectMeta` boilerplate in tests and production helpers.

#### Scope

Potentially add inherent methods in:

- `crates/flotilla-resources/src/resource.rs`
- optionally `crates/flotilla-resources/src/controller/mod.rs`

#### Suggested APIs

- `InputMeta::named(name)`
- `InputMeta::with_labels(...)`
- `InputMeta::with_annotations(...)`
- `InputMeta::with_owner_references(...)`
- `InputMeta::with_finalizers(...)`

And maybe:

- `ControllerObjectMeta::named(name)`
- `ControllerObjectMeta::owned_by(...)`

#### Notes

Keep this minimal. Do not introduce a large builder API unless the first small step proves insufficient.

#### Expected payoff

Reduces both test and production boilerplate, especially for child object creation.

#### Acceptance criteria

- at least 5 manual `InputMeta { ... }` blocks are removed
- constructors improve readability instead of hiding meaning

---

### 8. Reassess verbose nested spec construction after helper extraction

#### Goal

Decide whether derive macros are still needed once helpers exist.

#### Scope

After items 1-7, reassess hotspots such as:

- `WorkflowTemplateSpec`
- `TaskDefinition`
- `ProcessDefinition`
- `PlacementPolicySpec`

#### Recommendation

Do this last.

Expectation:

- helper constructors and fixtures remove most of the current pain
- builder derives may only be justified for especially deep nested test fixtures

#### If still needed

Trial a small, local use of a crate such as:

- `typed-builder`

Prefer:

- test-only or narrow use
- not broad builder proliferation across the model layer

#### Acceptance criteria

- only introduce builder macros if they clearly reduce code and improve readability in real examples
- avoid turning every type into a builder by default

## Execution order

### Phase 1: fastest wins

1. extract shared metadata and resource test helpers
2. extract daemon runtime `TestGitRepo`
3. add a controller-loop test harness

These are low-risk changes that should shrink code quickly.

### Phase 2: deduplicate tests

4. build generic backend contract tests
5. parameterize `task_workspace_reconciler.rs`

### Phase 3: improve test boundaries

6. move selected happy-path reconciler tests outward
7. reassess remaining unit tests for deletion or merge

### Phase 4: optional polish

8. add minimal production constructors for metadata
9. reevaluate derive/builders only if still warranted

## Guardrails

To keep this aligned with the current project needs:

- no architectural rewrites
- no changing controller/resource boundaries just to save lines
- prefer helpers over introducing new abstraction layers with their own concepts
- prefer behavior-level tests where they genuinely reduce duplication
- do not introduce builder crates until helper extraction has been tried first

## Success metrics

Good signs this cleanup worked:

- fewer local helper functions per test file
- fewer inline `InputMeta { ... }` and status-seeding blocks
- fewer tests asserting patch variants when status or child-resource behavior would suffice
- shorter daemon runtime test module
- new tests are cheaper to write than copying an existing test and editing it

## Execution checklist with rough effort and dependencies

This section turns the plan into an ordered set of concrete cleanup tasks.

Effort sizing is approximate:

- **S**: small, localized change
- **M**: medium refactor with a few touch points
- **L**: broader cleanup likely best tackled in stages

### Recommended sequence

1. **Task E: Daemon runtime git test helper**
2. **Task A: Shared resource-fixture helpers**
3. **Task D: Controller-loop harness**
4. **Task B: Generic resource backend contract tests**
5. **Task C: Task workspace reconciler test compaction**
6. **Task F: Reconciler test boundary cleanup**
7. **Task G: Minimal metadata constructors**
8. **Task H: Evaluate builder macro use**

### Task dependency summary

- **Task E**: independent
- **Task A**: independent, but helps nearly every later task
- **Task D**: depends lightly on Task A if shared wait helpers are reused
- **Task B**: benefits from Task A, but can start independently
- **Task C**: benefits strongly from Task A
- **Task F**: benefits from Tasks A and D, and partly from C
- **Task G**: should happen after seeing whether test-only helpers already remove enough boilerplate
- **Task H**: only after A-G, when the remaining pain is clear

### Task-by-task execution notes

#### Task E: Extract a daemon runtime git test helper

- **Effort:** S
- **Dependencies:** none
- **Why first:** localized, low-risk, and immediately reduces one very dense test module
- **Primary file targets:**
  - `crates/flotilla-daemon/src/runtime.rs`
- **Approach:**
  - add `TestGitRepo` helper in the runtime test module or a nearby test-only helper module
  - migrate all inline git bootstrap sequences in `crates/flotilla-daemon/src/runtime.rs`
  - keep behavior unchanged
- **Done when:** runtime tests read in terms of scenario setup instead of raw git plumbing

#### Task A: Build shared resource-fixture helpers

- **Effort:** M
- **Dependencies:** none
- **Why early:** unlocks later compaction work across multiple files
- **Primary file targets:**
  - `crates/flotilla-resources/tests/common/mod.rs`
  - `crates/flotilla-controllers/tests/common/mod.rs`
  - `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
  - `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
  - `crates/flotilla-resources/tests/provisioning_resources_in_memory.rs`
  - `crates/flotilla-resources/tests/controller_loop.rs`
- **Approach:**
  - expand `crates/flotilla-resources/tests/common/mod.rs`
  - expand `crates/flotilla-controllers/tests/common/mod.rs`
  - migrate a few highest-duplication files first
- **Done when:** repeated local create/status/meta helpers disappear from several files

#### Task D: Build a controller-loop harness

- **Effort:** M
- **Dependencies:** ideally after Task A
- **Why here:** once common setup helpers exist, loop lifecycle boilerplate becomes the next obvious source of noise
- **Primary file targets:**
  - `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
  - `crates/flotilla-resources/tests/controller_loop.rs`
  - `crates/flotilla-controllers/tests/common/mod.rs`
- **Approach:**
  - add a small harness for spawning loops and cleaning them up
  - consolidate `wait_until` helpers
  - migrate the main controller-loop-heavy tests onto it
- **Done when:** tests stop manually repeating spawn/abort/wait patterns

#### Task B: Build generic resource backend contract tests

- **Effort:** M
- **Dependencies:** benefits from Task A but can proceed independently
- **Why now:** after helpers and harness cleanup, repeated backend semantics are the next broad duplication hotspot
- **Primary file targets:**
  - `crates/flotilla-resources/tests/in_memory.rs`
  - `crates/flotilla-resources/tests/workflow_template_in_memory.rs`
  - `crates/flotilla-resources/tests/common/mod.rs`
  - optionally later: `crates/flotilla-resources/tests/http_wire.rs`
- **Approach:**
  - introduce one reusable contract module
  - migrate in-memory resource tests first
  - leave `http_wire.rs` as a follow-up if it complicates the first pass
- **Done when:** shared CRUD/watch semantics are expressed once for multiple resources

#### Task C: Compact task workspace reconciler tests

- **Effort:** M
- **Dependencies:** strongly benefits from Task A
- **Why at this point:** fixture helpers should make it easier to see the remaining genuine complexity vs duplication
- **Primary file targets:**
  - `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
  - `crates/flotilla-controllers/tests/common/mod.rs`
- **Approach:**
  - convert repeated strategy/setup variants into table-driven helpers
  - keep only distinct edge-case tests as standalone functions
- **Done when:** `crates/flotilla-controllers/tests/task_workspace_reconciler.rs` is meaningfully shorter and easier to scan

#### Task F: Clean up reconciler test boundaries

- **Effort:** M to L
- **Dependencies:** benefits from Tasks A, C, and D
- **Why later:** easier once behavior-level tests are already cheaper to express
- **Primary file targets:**
  - `crates/flotilla-controllers/tests/clone_reconciler.rs`
  - `crates/flotilla-controllers/tests/environment_reconciler.rs`
  - `crates/flotilla-controllers/tests/checkout_reconciler.rs`
  - `crates/flotilla-controllers/tests/terminal_session_reconciler.rs`
  - `crates/flotilla-controllers/tests/provisioning_in_memory.rs`
- **Approach:**
  - identify happy-path unit tests in the reconciler-specific files
  - replace or merge them into controller-loop tests where appropriate
  - retain unit tests for naming, validation, and failure mapping
- **Done when:** unit tests skew toward edge cases, while happy paths are covered through observable controller behavior

#### Task G: Add minimal metadata constructors

- **Effort:** S to M
- **Dependencies:** best after A-F
- **Why later:** only worth doing once it is clear that helper extraction did not already remove enough of the pain
- **Primary file targets:**
  - `crates/flotilla-resources/src/resource.rs`
  - optionally `crates/flotilla-resources/src/controller/mod.rs`
  - follow-up adoption sites in test helpers and child-resource creation paths
- **Approach:**
  - add a minimal set of inherent constructors on `InputMeta`
  - optionally add a minimal constructor for `ControllerObjectMeta`
  - migrate a handful of the clearest call sites first
- **Done when:** obvious metadata boilerplate is reduced without introducing a large API surface

#### Task H: Evaluate builder macro usage

- **Effort:** S for evaluation, M if adopted narrowly
- **Dependencies:** after all earlier cleanup
- **Why last:** helper extraction may make this unnecessary
- **Primary file targets:**
  - likely hotspots include:
    - `crates/flotilla-resources/tests/common/mod.rs`
    - `crates/flotilla-controllers/tests/task_workspace_reconciler.rs`
    - `crates/flotilla-resources/src/workflow_template.rs`
    - `crates/flotilla-resources/src/convoy.rs`
- **Approach:**
  - first document remaining verbose construction hotspots
  - if still justified, trial a narrow use of `typed-builder` in one painful area
  - compare before/after readability explicitly in the change description
- **Done when:** either builders are ruled out with confidence or adopted in a very limited, clearly beneficial way
