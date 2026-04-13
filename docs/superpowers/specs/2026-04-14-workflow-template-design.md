# WorkflowTemplate Resource — Design

## Context

WorkflowTemplate is Stage 2 of the convoy implementation (see `docs/superpowers/specs/2026-04-13-convoy-brainstorm-prompts.md`). It defines the shape of a workflow as a DAG of tasks, separate from any convoy instance. Convoys (Stage 3) reference WorkflowTemplates and instantiate them with concrete inputs and placement.

WorkflowTemplate is a **pure data resource** in this stage: no controller, no status subresource, no observed state. Consumers (the convoy controller, CLI tools) validate templates client-side. A status+controller shape may be added later when multiple authors on shared clusters or cross-template references make server-side validity tracking worthwhile.

## Crate

Lives in the existing `crates/flotilla-resources` crate alongside the convoy CRD. Uses the `Resource` trait from Stage 1. No new crate needed.

## Scope

### In scope

- Rust type `WorkflowTemplate` with `Spec = WorkflowTemplateSpec` and `Status = ()`.
- Hand-written CRD YAML (`src/crds/workflow_template.crd.yaml`), namespaced.
- `validate(spec) -> Result<(), Vec<ValidationError>>` library function covering DAG and schema-semantic rules.
- Round-trip tests against in-memory and HTTP backends.
- An example binary extension that applies the CRD and exercises CRUD + watch.
- A small `validate <path>` CLI that runs validation without a cluster.

### Out of scope (for this stage)

- Convoy controller or any controller at all.
- Selector resolution, prompt rendering, variable interpolation.
- Status subresource, admission webhooks, server-side validation.
- Presentation/layout configuration.
- Compatibility with today's `WorkspaceTemplate` (`.flotilla/workspace.yaml`). We are in a no-backwards-compatibility phase; new workflows are authored fresh.

## Resource Definition

### Rust

```rust
pub struct WorkflowTemplate;

impl Resource for WorkflowTemplate {
    type Spec = WorkflowTemplateSpec;
    type Status = ();

    const API_PATHS: ApiPaths = ApiPaths {
        group: "flotilla.work",
        version: "v1",
        plural: "workflowtemplates",
        kind: "WorkflowTemplate",
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTemplateSpec {
    #[serde(default)]
    pub inputs: Vec<InputDefinition>,
    pub tasks: Vec<TaskDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDefinition {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub processes: Vec<ProcessDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessDefinition {
    pub role: String,
    #[serde(flatten)]
    pub source: ProcessSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ProcessSource {
    Agent { selector: Selector, #[serde(default)] prompt: Option<String> },
    Tool  { command: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selector {
    pub capability: String,
}
```

### YAML

```yaml
apiVersion: flotilla.work/v1
kind: WorkflowTemplate
metadata:
  name: review-and-fix
  namespace: flotilla
spec:
  inputs:
    - name: feature
      description: Brief description of the feature to implement
    - name: branch
      description: Target git branch

  tasks:
    - name: implement
      processes:
        - role: coder
          selector: { capability: code }
          prompt: |
            Implement {feature} on branch {branch}.
            Push commits as you go.

        - role: build
          command: "cargo watch -x check"

    - name: review
      depends_on: [implement]
      processes:
        - role: reviewer
          selector: { capability: code-review }
          prompt: "Review the branch {branch} for correctness and style."

        - role: tests
          command: "cargo test --watch"
```

### Notes on shape

- **`status: ()`** because WorkflowTemplate has no observed state. If that changes later (validity tracking, reference-counting), widen the type.
- **`#[serde(flatten, untagged)]`** on `ProcessSource` gives the verbatim YAML shape — `selector` present → `Agent`, `command` present → `Tool`. No explicit `kind:` discriminator is required.
- **Task list order** is an authoring convenience; execution order comes from `depends_on` alone.
- **Commands and prompts are opaque strings.** `{var}` placeholders are permitted and must reference declared inputs, but substitution happens at convoy launch (Stage 3), not here.
- **`role` uniqueness is per-task, not per-workflow.** Two tasks can each have a `coder`; they are different process instances at different times.
- **`deny_unknown_fields` on `ProcessSource`.** Without it, serde's untagged decode silently drops unknown fields — a `prompt:` alongside `command:` would deserialise as a tool process and discard the prompt. `deny_unknown_fields` surfaces the mismatch as a parse error, which authoring agents can act on.

## CRD YAML

`crates/flotilla-resources/src/crds/workflow_template.crd.yaml`. Namespaced, group `flotilla.work`, v1, no status subresource.

```yaml
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: workflowtemplates.flotilla.work
spec:
  group: flotilla.work
  scope: Namespaced
  names:
    plural: workflowtemplates
    singular: workflowtemplate
    kind: WorkflowTemplate
    shortNames: [wft]
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
              required: [tasks]
              properties:
                inputs:
                  type: array
                  items:
                    type: object
                    required: [name]
                    properties:
                      name: { type: string, minLength: 1 }
                      description: { type: string }
                      required: { type: boolean, default: true }
                tasks:
                  type: array
                  minItems: 1
                  items:
                    type: object
                    required: [name, processes]
                    properties:
                      name: { type: string, minLength: 1 }
                      depends_on:
                        type: array
                        items: { type: string }
                      processes:
                        type: array
                        minItems: 1
                        items:
                          type: object
                          required: [role]
                          properties:
                            role: { type: string, minLength: 1 }
                            selector:
                              type: object
                              required: [capability]
                              properties:
                                capability: { type: string, minLength: 1 }
                            prompt: { type: string }
                            command: { type: string }
                          oneOf:
                            - required: [selector]
                            - required: [command]
```

- `oneOf` enforces the `selector` XOR `command` rule at the API layer.
- `shortNames: [wft]` gives `kubectl get wft`.
- Structural constraints only — semantic rules (cycles, missing deps, unknown input references) live in Rust.

## Validation

### API

```rust
pub fn validate(spec: &WorkflowTemplateSpec) -> Result<(), Vec<ValidationError>>;

pub enum ValidationError {
    DuplicateTaskName { name: String },
    DuplicateRoleInTask { task: String, role: String },
    UnknownDependency { task: String, missing: String },
    DependencyCycle { cycle: Vec<String> },
    ProcessMissingSource { task: String, role: String },
    ProcessBothSources { task: String, role: String },
    UnknownInputReference { task: String, role: String, input: String },
    DuplicateInputName { name: String },
}
```

One pass, returns **all** errors. Agents authoring templates benefit from seeing the full set rather than fixing one at a time.

### Rules

- Task names unique within a workflow.
- Role names unique within a task.
- Every `depends_on` entry resolves to an existing task name.
- No cycles; reported with the cycle path (`[a, b, c, a]`).
- Each process has exactly one source: `selector` (agent) or `command` (tool). Serde's untagged decode enforces this structurally; the validator double-checks with a clearer error.
- `{name}` placeholders in any prompt or command must match a declared input name. Literal brace escape (`{{`) is deferred unless a real command needs it.
- Input names unique.

### Where validation runs

- **Client-side before `kubectl apply`**, via the `flotilla-resources validate <path>` CLI or a future higher-level `flotilla` subcommand.
- **Inside any consumer on read** (Stage 3's convoy controller will re-validate before using a template).
- **Not** via a status-writing controller in this stage. See the deferred list.

## Tests

### Unit tests

- One validation fixture per `ValidationError` variant, verifying the right error is produced.
- Parser round-trip: the sample YAML in this document deserialises into the expected Rust shape, then re-serialises to equivalent YAML.

### In-memory backend tests

Mirroring the existing convoy tests: create → get → list → watch → update → delete.

### HTTP backend integration tests

Against minikube, same pattern as convoy:
- Bootstrap: apply the WorkflowTemplate CRD via `ensure_crd`.
- CRUD + watch round-trip.
- Run only when minikube is reachable (same gate as existing integration tests).

## Example Binary and CLI

Extend the existing `examples/k8s_crud.rs` to:

1. Apply both the Convoy and the WorkflowTemplate CRDs.
2. Load `examples/review-and-fix.yaml` (sample WorkflowTemplate), validate it, and report any errors.
3. Create the WorkflowTemplate in the cluster.
4. List and re-read it.

A small `cargo run --example validate_workflow -- <path>` (or a short binary target) loads a YAML file and runs `validate()` without touching a cluster. Exit code reflects success/failure; output lists all errors.

## Deliverables

1. `WorkflowTemplate` Rust type + `Resource` impl.
2. `WorkflowTemplateSpec`, `TaskDefinition`, `ProcessDefinition`, `ProcessSource`, `Selector`, `InputDefinition` types.
3. `validate()` library function with the `ValidationError` enum.
4. `src/crds/workflow_template.crd.yaml` — hand-written CRD.
5. Round-trip tests against in-memory and HTTP backends.
6. Validation unit tests covering each error variant.
7. Example binary extension exercising CRUD + watch against minikube.
8. `validate <path>` CLI entry point that runs without a cluster.

No controller. No selector resolution. No rendering. No convoy integration. Those land in Stage 3 and later.

## Design Decisions

### Untagged `selector` XOR `command`

Matches the design doc's YAML shape verbatim and reads naturally when authored by humans or agents. Serde's `#[serde(flatten, untagged)]` disambiguates on field presence. The CRD's `oneOf` enforces the constraint at the API layer; the Rust validator double-checks with clearer errors.

### No status, no controller

The brainstorm prompt framed WorkflowTemplate as "pure data, no controller needed." We kept that framing. `validate()` is a pure function over the spec; any consumer can run it on read. Adding a status subresource now without a writer would be misleading; adding a writer would inflate Stage 2 beyond its scope. The "fail fast at apply" UX is better served by a client-side CLI validator than by a status subresource.

### Inputs declared at workflow level

`inputs:` makes it explicit what a convoy must supply at launch. This lets validation check that `{var}` references are resolvable without running the convoy, and documents the template's interface. `required: false` covers truly optional inputs. Multi-valued / richer-typed inputs are deferred.

### Prompts on agent processes

Capability says *who*; prompt says *what to do*. Without prompts, agent templates would be unusable — every real use case has some instruction, even if the instruction is "review the branch." Deferring prompts would have forced every consumer to invent its own mechanism.

### Extensible `ProcessSource`

Two variants (`Agent`, `Tool`) cover Stage 2. A future `AgentRef { agent, resume, prompt }` variant plus a workflow-level `agents:` declaration can slot in when agent lifetime / re-entry semantics are understood. The untagged enum remains extensible without breaking existing templates.

### No layout in the template

Content and layout were conflated in today's `WorkspaceTemplate`. The convoy design deliberately splits them: WorkflowTemplate defines *what runs*, PresentationManager decides *how it's arranged*. Presentation configuration (and, eventually, a Yeoman agent that learns user preferences and watches presentation events) lives in a separate layer.

### Namespaced CRD

Matches Convoy, matches k8s convention for user-defined resources. Per-team customization is natural; cluster-scoped templates (shared libraries, Argo-style `ClusterWorkflowTemplate`) can be added later if the use case appears.

### Client-side DAG validation, not CRD OpenAPI

CRD OpenAPI can enforce structural shape (required fields, types, `oneOf`) but cannot express "no cycles" or "all `depends_on` names resolve." Those live in Rust. A validator agent or admission webhook can surface them server-side later.

## Deferred Items

Captured in `docs/superpowers/specs/2026-04-13-convoy-brainstorm-prompts.md` under "Deferred Items → From Stage 2":

- Loops / retry edges (review → fix cycles).
- Conditional edges (approval gates).
- User tasks (human actions inside a DAG).
- Named artifacts / data flow (one task produces a value, downstream consumes it; branch-naming is the canonical case).
- Agent lifetime across tasks (resume, session continuity).
- One-shot agent processes (non-long-running agents that produce a value, e.g. haiku branch-namer).
- Multi-valued / richer inputs (starting from N issues; typed inputs, defaults).
- Non-terminal content (port-forwarding for dev servers, background services, HTTP probes).
- GitOps sync (templates authored in VCS, synced by an Argo CD / Flux style controller).
- Status subresource + validator controller — likely the right end state once templates reference each other or shared-cluster authoring demands fast-feedback validity.
