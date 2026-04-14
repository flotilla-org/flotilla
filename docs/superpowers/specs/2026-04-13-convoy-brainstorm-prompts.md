# Convoy Implementation — Brainstorm Prompts

These are self-contained prompts for future brainstorm sessions, ordered by dependency. Reference the design doc at `docs/superpowers/specs/2026-04-13-convoy-and-control-plane-design.md` for full context.

## Dependency Graph

```
1. ResourceClient trait + k8s REST prototype
2. WorkflowTemplate resource                   depends on: 1
3. Convoy resource + controller                 depends on: 1, 2
4. Task provisioning / policy                   depends on: 3
5. Presentation integration + rename            depends on: 3
6. TUI convoy view                              depends on: 3
7. AttachableSet migration                      depends on: 3, 4, 5, 6
```

Stages 4, 5, and 6 can run in parallel once 3 is complete.

## Design Decisions

### kube-rs vs raw REST

We prefer **raw REST calls** (reqwest + serde) over kube-rs for the k8s backend. Reasons:
- kube-rs is a heavy dependency with opinions about async runtime
- The `ResourceClient` trait should reflect plain REST semantics (what flotilla-cp would eventually expose), not kube-rs's `Api<T>` abstractions
- For prototyping a single-node controller loop, a simple `loop { watch, react }` is clearer than kube-rs's reconciler framework
- kube-rs doesn't provide leader election (community crate, no fencing) — not needed for single-node anyway
- We already have reqwest and serde

### Hand-written CRD YAML over macro generation

CRD specs are written as plain YAML, not generated from Rust derive macros (e.g. kube-rs `#[derive(CustomResource)]`). Reasons:
- CRD YAML diffs are readable — you see exactly what changed in the schema
- Macro-generated CRD output is opaque — derive attribute changes don't show the actual schema effect
- k8s CRD specs are full of `x-kubernetes-*` annotations and structural schema requirements that fight macro generation
- Hand-written YAML is debuggable with `kubectl apply --dry-run` and standard k8s tooling

### Pure k8s prototype (stages 1-4)

Stages 1 through 4 should be prototyped as a standalone project against real k8s, using pure k8s primitives. The goal is to understand the CRD/controller model with real tooling feedback (`kubectl get convoys`, watch events). This prototype is separate from the flotilla codebase — it validates the resource model before integrating.

Task provisioning (stage 4) is where the interesting policy question lives: should tasks create k8s-native sub-resources (Pods for containerized agents, Jobs for one-shot tasks) and let built-in k8s controllers handle lifecycle? Or should our controller manage everything directly? This is a deployment policy choice — the same convoy controller should work either way. The prototype will help answer this by trying both.

### Correlation is downstream

Convoy data feeds into the existing provider data / correlation engine as just another data source. The convoy doesn't need to know about correlation — it emits items with appropriate keys, and the correlation engine groups them with independently-discovered PRs, branches, etc. This means convoy integration with the current model is thin: convoys produce `ProviderData` items, correlation handles the rest. The exact shape of correlation in the long term is a separate concern.

---

## 1. ResourceClient Trait and k8s REST Backend

**Goal:** Define the `ResourceClient` trait that convoy controllers use, and implement it against real k8s via raw REST calls.

**Context:** Flotilla's convoy system needs a k8s-style resource API (get/list/watch/create/update/delete with resourceVersion). Controllers are written against a trait so they can run against k8s REST (prototyping), a future flotilla-cpHTTP backend, or InProcessflotilla-cp (zero-dependency laptop case). See the flotilla-cp design doc for the full vision.

This should be prototyped as a **standalone project** outside the flotilla codebase — a minimal Rust binary that registers CRDs and does CRUD + watch against a local k8s cluster (minikube). k3s is not viable (Linux-only); Go's goroutine stack model prevents embedding any Go-based k8s API server as a library, so the production in-process resource server will be a Rust/SQLite reimplementation of the required subset.

**Key questions:**
- What's the minimal trait surface that convoy controllers actually need? Start from usage, not from "what k8s has."
- How does watch work in the trait? The k8s watch API returns newline-delimited JSON with ADDED/MODIFIED/DELETED events. Streaming iterator? Async channel?
- How are resource types represented? Generic `ResourceClient<T: Resource>` with serde, or runtime-typed with serde_json::Value?
- Resource metadata subset: name, namespace, resourceVersion, labels are likely needed. Annotations? Generation/observedGeneration?
- Error model: Conflict (resourceVersion mismatch), NotFound, etc. What does the trait's error type look like?
- How much k8s discovery/negotiation is needed for raw REST? CRD registration, API group discovery, OpenAPI — what's the minimum to make `kubectl get convoys` work?

**Constraints:**
- The trait must be testable with an in-memory implementation (tests shouldn't need a running cluster).
- Must support watch (convoy controller needs to react to status changes).
- Keep it minimal — we can add methods as controllers need them.
- Use reqwest + serde, not kube-rs.

**Starting point:** k8s API reference for custom resources. The k8s watch protocol. Look at `AttachableStoreApi` in `crates/flotilla-core/src/attachable/store.rs` for the current CRUD pattern we're replacing.

---

## 2. WorkflowTemplate Resource

**Goal:** Define the WorkflowTemplate resource type — the reusable DAG definition that convoys instantiate.

**Context:** A WorkflowTemplate defines the shape of a workflow: named tasks, their process definitions (role + command), and dependency edges between tasks. It's a pure data resource (no controller needed). Templates don't specify where things run (host/checkout/environment) — that's resolved at convoy launch time.

Today's `WorkspaceTemplate` (`.flotilla/workspace.yaml`) defines content (roles + commands) and layout (pane arrangement) for a single task. WorkflowTemplate generalizes this to a DAG of tasks. Single-task workflows should be backwards-compatible with the workspace template experience.

Part of the standalone k8s prototype from stage 1.

**Key questions:**
- What's the YAML format? How does it relate to/differ from the existing workspace.yaml?
- How do single-task workflows degrade gracefully to the current workspace template experience?
- Where do templates live? `.flotilla/workflows/` in repo? Global config? Both with precedence? As k8s resources (like ConfigMaps)?
- How are process definitions expressed? Just `role + command` for now, but what's the extension point for future agent configuration (system prompts, permissions, hooks)?
- How are DAG edges expressed? `depends_on: [task_name]` is simple — is it sufficient?
- How does template rendering/variable substitution work? Current `WorkspaceTemplate::render()` does simple `{var}` replacement.
- Validation: what makes a template invalid? (cycles in DAG, duplicate task names, missing dependency references)

**Starting point:** Read `crates/flotilla-core/src/template.rs` for the current `WorkspaceTemplate`. Read the convoy design doc's WorkflowTemplate section. Look at Argo Workflow's template model for comparison (useful reference, but we want something lighter).

---

## 3. Convoy Resource and Controller

**Goal:** Define the Convoy resource type and build a minimal controller that manages convoy lifecycle through the workflow DAG.

**Context:** A Convoy is a named workflow instance. It references a WorkflowTemplate, tracks task states, and advances through the DAG as tasks complete. This is the core of the new model — it replaces the "branch -> checkout -> terminals" interaction loop with "convoy -> (provisions everything needed) -> work happens."

Part of the standalone k8s prototype. The controller should be a simple reconciliation loop: watch convoy resources, compare desired state (workflow DAG) against observed state (task statuses), take action.

**Key questions:**
- Convoy spec: what fields? Name, workflow reference, per-task overrides (host/checkout/environment bindings)?
- Convoy status: task states, overall phase, timestamps?
- How is a convoy created? For the prototype: `kubectl apply`. For flotilla: CLI command, TUI action, eventually agent-initiated.
- The controller reconciliation loop: what does it watch, what does it do on each tick?
  - Convoy created -> mark root tasks as Ready
  - Task marked Ready -> (hand off to task provisioning, brainstorm 4)
  - Task completed -> check dependents, mark newly-unblocked tasks as Ready
  - All tasks completed -> mark convoy Completed
- How does task completion work? Initially explicit (CLI command, status patch). Eventually agents mark their own tasks complete.
- Should tasks be sub-resources of the convoy (status fields) or independent resources? Status fields are simpler for now. Independent resources give you watch/status per task but add complexity.
- How does the controller get started? In the prototype: standalone binary. In flotilla: part of the daemon.
- This controller pattern should eventually replace much of the current command/step/executor complexity. How does the reconciliation model compare to the current `StepPlan` execution?

**Dependencies:** ResourceClient trait (brainstorm 1), WorkflowTemplate (brainstorm 2).

**Starting point:** Read `crates/flotilla-core/src/executor/workspace.rs` for the current orchestration flow. Read `crates/flotilla-core/src/step.rs` for the current step execution model that convoys would replace. Read the convoy design doc.

---

## 4. Task Provisioning and Policy

**Goal:** When a convoy task becomes Ready, provision the infrastructure it needs. Determine the policy boundary: what does the convoy controller create directly vs. what does it delegate to other controllers?

**Context:** This is where the convoy controller creates real things. The interesting question is the policy layer: should the controller create k8s-native resources (Pods, Jobs) and let built-in controllers handle lifecycle? Or create flotilla-specific resources and manage them directly?

For example, a task needing a containerized coding agent could:
- **k8s-native**: Create a Pod spec -> k8s schedules it -> container runs agent -> controller watches Pod status
- **flotilla-managed**: Controller calls EnvironmentProvider + TerminalPool directly -> creates terminal sessions -> watches terminal status

The k8s-native path gives you scheduling, restarts, resource limits for free. The flotilla-managed path works on a laptop without k8s. The convoy controller shouldn't care which — it creates a "task execution" resource and watches its status. The policy layer decides how that resolves.

**Key questions:**
- What's the provisioning sequence for a task? Environment first, then checkout accessibility, then processes?
- The policy boundary: what abstraction separates "what the task needs" from "how it's provided"? Is this just the ResourceClient creating different resource types depending on policy?
- For the k8s prototype: try creating Pods for tasks. What works well? What's awkward?
- For the laptop case: the same task definition should resolve to local processes via TerminalPool. How does the policy switch work?
- Host assignment: explicit in convoy spec? Defaulted? Eventually scheduled?
- Failure handling: provisioning fails — what happens to the task? Retry? Mark failed? Propagate to convoy?
- How does this relate to the current executor's command flow? The `WorkspaceOrchestrator` does: ensure environment -> ensure checkout -> ensure terminals -> bind to workspace. Is this the same sequence, just expressed as resource creation + reconciliation?

**Dependencies:** Convoy resource + controller (brainstorm 3).

**Starting point:** Read `crates/flotilla-core/src/executor/workspace.rs` (WorkspaceOrchestrator), `crates/flotilla-core/src/executor/session_actions.rs`, `crates/flotilla-core/src/hop_chain/`, and `crates/flotilla-core/src/step.rs`.

---

## 5. Presentation Integration and PresentationManager Rename

**Goal:** Rename WorkspaceManager to PresentationManager, and connect convoy tasks to the presentation layer so users can see and interact with running processes.

**Context:** The presentation manager already receives fully-resolved `Vec<(role, command_string)>` pairs and a layout template — it doesn't care where commands came from. For a single convoy task, the existing `WorkspaceAttachRequest` -> `resolve_template` -> `PaneLayout` -> create panes path works unchanged.

The new parts are:
- **Rename**: WorkspaceManager -> PresentationManager throughout the codebase. Mechanical but touches many files. The user-facing concept of "workspace" (tmux/zellij workspace) may stay — the rename is internal.
- **Task transitions**: when task A completes and task B starts, the presentation should update (add/replace panes rather than creating a new workspace).
- **Convoy TUI pane**: a flotilla TUI instance focused on the convoy, running as a pane in the presentation alongside terminal processes, showing task DAG progression.

**Key questions:**
- Reconfigure vs replace: when a new task starts, should the existing workspace gain new panes, or should a new workspace replace it? Reconfiguration needs new `update_workspace`/`add_panes` on the trait.
- What does the presentation layout look like for a multi-task convoy? One workspace for the whole convoy? One per task?
- How does the flotilla TUI pane work? It's another process in the layout — `flotilla tui --convoy <name>`? How does it get convoy state?
- Layout config: per-task? Convoy-level? Both with defaults?
- Should user-facing terminology stay as "workspace" while the internal trait is "PresentationManager"?

**Dependencies:** Convoy resource + controller (brainstorm 3).

**Starting point:** Read `crates/flotilla-core/src/providers/workspace/mod.rs` (resolve_template, build_pane_layout, WorkspaceManager trait), `crates/flotilla-core/src/providers/workspace/cmux.rs` (create_workspace implementation). Grep for `workspace_manager` in config and binding strings.

---

## 6. TUI Convoy View

**Goal:** Add convoy-aware views to the flotilla TUI — showing convoy status, task DAG progression, and navigating to task processes.

**Context:** Today the TUI shows work items (correlated groups of checkouts, PRs, sessions, terminals). The convoy becomes the primary object the user works with. The TUI needs to show:

- Convoy list (like work item table but convoy-focused)
- Convoy detail: task DAG with status indicators (Pending/Ready/Running/Completed)
- Task detail: processes, their terminal status, links to attach
- Convoy creation flow (eventually)

There's also the "convoy pane" concept from brainstorm 5: a flotilla TUI instance running inside the presentation workspace, showing convoy progression alongside terminal processes.

Convoy data feeds into the TUI through the normal provider data / snapshot path. The convoy controller produces items that become part of `ProviderData`, and correlation groups them with related work items (PRs, branches) downstream. The TUI doesn't need special convoy awareness in the correlation layer — it just needs to display convoy-shaped data well.

**Key questions:**
- Does the convoy view replace the work item table, supplement it, or is it a different tab/mode?
- How does the TUI get convoy state? Through the existing snapshot/provider data path? Or a dedicated convoy watch?
- What does the DAG visualization look like in a terminal? Simple list with indentation and status icons? Actual graph rendering?
- Task interaction: how does the user mark a task complete? Action menu? Keybinding?
- How does "attach to process terminal" work from the convoy view?
- The convoy-as-TUI-pane: is this `flotilla tui --filter convoy=<name>`? A separate subcommand?

**Dependencies:** Convoy resource + controller (brainstorm 3).

**Starting point:** Read `crates/flotilla-tui/src/widgets/repo_page.rs`, `crates/flotilla-tui/src/app/mod.rs`, `crates/flotilla-core/src/data.rs` (work item building).

---

## 7. AttachableSet Migration

**Goal:** Migrate existing AttachableSet functionality to convoys, then remove the old model.

**Context:** Once convoys handle the full lifecycle (create, provision, present, tear down), AttachableSet becomes redundant. Today's AttachableSet is a convoy with a single task — the migration should be straightforward once the convoy model is solid.

**Key questions:**
- Can migration be gradual? (new code paths create convoys, old code paths still create AttachableSets, both coexist temporarily)
- What happens to persisted AttachableSet state in `~/.config/flotilla/attachables/registry.json`? Auto-migrate on startup? Clean break?
- The binding system (`ProviderBinding`) — does it have a convoy equivalent, or do native resource references replace it entirely?
- What correlation keys do convoys emit? The existing `CorrelationKey::AttachableSet(id)` needs a convoy equivalent, but the exact shape depends on how correlation evolves. Keep it simple: convoy tasks emit the same keys their sub-resources would (Branch, CheckoutPath, etc.) and correlation groups them naturally.

**Dependencies:** All previous brainstorms (3-6) should be solid before migrating.

**Starting point:** Read `crates/flotilla-core/src/attachable/` (full module), `crates/flotilla-core/src/providers/correlation.rs`.

---

## Deferred Items

Things deliberately pushed out of earlier stages. Each stage's spec should restate the relevant ones, but the master list lives here so nothing falls through.

### From Stage 2 (WorkflowTemplate)

- **Loops / retry edges** — review → fix → review cycles. A new edge kind beyond `depends_on` is likely needed. See the WorkflowTemplate spec for details.
- **Conditional edges** — approval gates ("proceed only if reviewer approves").
- **User tasks** — a task whose "process" is a human action (confirm a generated branch name, approve a spec). Could be a new `ProcessSource` variant or a task-level `user_prompt:`.
- **Named artifacts / data flow** — a task produces a value, downstream tasks consume it. Motivating example: a one-shot haiku call generates a branch name, downstream tasks use `{branch}`. Today's branch-name-generation-plus-confirmation flow is the concrete case to design around.
- **Agent lifetime across tasks** — resume/session continuity. `implement → review → fix` wants the same coding agent, not a fresh one. Runtime semantics are unclear (same-process send-keys vs session resume vs fresh-but-context-aware), so schema shape is deferred. The `ProcessSource` enum is kept extensible so an `AgentRef { agent, resume, prompt }` variant can slot in later alongside workflow-level `agents:` declarations.
- **One-shot agent processes** — non-long-running agents that produce a value. The haiku branch-namer is the canonical case. Distinct from long-running agent processes that stay up for interaction.
- **Optional and multi-valued / richer inputs** — starting a workflow from 0+ issues (each with number, title, body, URL), not just a scalar string. Also: default values, typed inputs, `required: false`. Runtime semantics likely follow Argo's "absent value ⇒ empty string substitution." Deferred as a bundle because the runtime semantics and schema shape move together.
- **Additional interpolation scopes** — `{{tasks.<name>.outputs.*}}`, `{{items.*}}`, `{{workflow.creationTimestamp}}`, `{{workflow.uid}}`, etc. Tracks Argo's reference table as we add the corresponding runtime concepts.
- **Expression form `{{=...}}`** — Argo's expression mode (casts, filters, sprig functions). Useful once the simple form is paying its way.
- **Literal recognized-token escape** — a way to emit `{{inputs.branch}}` or `{{workflow.name}}` verbatim in prompts/commands. Foreign prefixes already pass through unchanged; this deferred item is only for flotilla-owned tokens.
- **Non-terminal content** — port-forwarding for dev servers, HTTP probes, background services. Process definitions are terminal-only in v1.
- **GitOps sync** — templates authored in project VCS, synced into the cluster by a controller (Argo CD / Flux style). Relevant when templates live in repos rather than being applied manually.

### From Stage 3 (Convoy resource + controller)

See `docs/superpowers/specs/2026-04-14-convoy-resource-design.md` for the spec.

- **`PlacementPolicy` resource** — named, default, auto-discovered (today's `docker@host` style). Eventually delegates to or is implemented by a `PersistentAgent`. Stage 3's Convoy references one by opaque string in `spec.placement_policy`; Stage 4 reifies.
- **`PersistentAgent` resource** — one resource type with k8s-style labels/selectors. Quartermaster, Yeoman, TestCoach, SecurityReviewer, etc. are conventionally-labeled instances. Agent runtime shape deliberately open: managed CLI (input-send), external CLI (shell-out), headless JSON/ACP, or internal LLM loop. All presentable — CLI by attaching terminals, others via interaction-log views.
- **Presentation-scope decoupling** — `PresentationManager` at full-flotilla / repo / convoy scopes, no longer coupled to a single workspace.
- **Interactive launch UX** — CLI/TUI flow: fetch template → infer inputs from context (current branch, selected issues) → present for approval → create convoy.
- **Typed `InputValue` variants** — `Issue`, `IssueList`, `Branch`, `ChangeRequest`. Requires matching `InputDefinition.kind` in `WorkflowTemplate` (Stage 2 revision). Enables UI-driven workflow discovery ("user has issues selected → find workflows that accept issue[]") and semantic downstream use (correlation, PR metadata).
- **Label-based workflow discovery** — alternative or complement to typed inputs; `flotilla.work/accepts: issue` label on `WorkflowTemplate`.
- **Workflow composition (`includes`)** — sub-workflows; transitive snapshotting into `observed_workflows` (the map shape in Stage 3 is already forward-compatible). Opens snapshot-at-root vs snapshot-per-include choice.
- **Template versioning / rev references** — `spec.workflow_ref_revision` for convoys that want a specific template version. Stage 3 just snapshots whatever was current at init.
- **Convoy re-run** — copy a convoy, reset status, re-snapshot against newer template. Not a common case, but useful enough to capture.
- **Convoy cancellation** — user-initiated cancel producing `ConvoyPhase::Cancelled`. The phase is reserved in Stage 3 but never produced.
- **Admission webhook / fast-feedback validation** — complements the client-side Convoy validator once shared-cluster authoring demands it.

### From Stage 4 (Task provisioning / policy)

*(To be filled in when Stage 4 is designed.)*
