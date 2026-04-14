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
- Selector resolution, prompt rendering, variable *substitution*. (Interpolation *syntax and reference validation* is in scope — resolving the values isn't.)
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
}

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
            Convoy {{workflow.name}} — implement {{inputs.feature}} on
            branch {{inputs.branch}}. Push commits as you go.

        - role: build
          command: "cargo watch -x check"

    - name: review
      depends_on: [implement]
      processes:
        - role: reviewer
          selector: { capability: code-review }
          prompt: "Review branch {{inputs.branch}} for correctness and style."

        - role: tests
          command: "cargo test --watch"
```

### Notes on shape

- **`status: ()`** because WorkflowTemplate has no observed state. If that changes later (validity tracking, reference-counting), widen the type.
- **`#[serde(flatten, untagged)]`** on `ProcessSource` gives the verbatim YAML shape — `selector` present → `Agent`, `command` present → `Tool`. No explicit `kind:` discriminator is required.
- **Task list order** is an authoring convenience; execution order comes from `depends_on` alone.
- **Commands and prompts both support `{{...}}` interpolation.** Collision with kubectl/Helm/Go-template snippets (`{{.metadata.name}}`, `{{ .Release.Name }}`) is avoided by a prefix allowlist: only tokens starting with a known scope (`inputs.`, `workflow.`) are recognized; any other token is left alone verbatim for downstream tooling to handle. Matches Argo's approach. Substitution happens at convoy launch (Stage 3); Stage 2 only validates that *recognized* references resolve.
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
                              not: { required: [command] }
                            - required: [command]
                              not:
                                anyOf:
                                  - required: [selector]
                                  - required: [prompt]
```

- The symmetric `oneOf`+`not` block enforces the full XOR: a process is either `{selector, prompt?}` or `{command}` — never both, and a `prompt` on a `command`-only process is rejected at the API layer (matching Rust's `deny_unknown_fields`).
- `shortNames: [wft]` gives `kubectl get wft`.
- Structural constraints only — semantic rules (cycles, missing deps, unknown input references) live in Rust.

## Parse vs. Validate

Two layers with distinct responsibilities:

- **Parse** (`serde_yml::from_str::<WorkflowTemplateSpec>`): catches *structural* errors — missing required fields, wrong types, a process with neither `selector` nor `command`, a process with both, unknown fields on `ProcessSource`. Serde reports the first structural error and stops.
- **Validate** (`validate(&WorkflowTemplateSpec)`): runs on a successfully-parsed spec. Catches *semantic* errors — cycles, unknown dependency names, duplicate task/role/input names, unresolved `{{...}}` references. Returns **all** semantic errors in a single pass so template authors see the full set.

The CLI runs parse first; if parsing fails, it reports the parse error and exits. If parsing succeeds, it runs `validate()` and reports every error it finds.

## Validation

### API

```rust
pub fn validate(spec: &WorkflowTemplateSpec) -> Result<(), Vec<ValidationError>>;

pub enum ValidationError {
    DuplicateTaskName { name: String },
    DuplicateRoleInTask { task: String, role: String },
    UnknownDependency { task: String, missing: String },
    DependencyCycle { cycle: Vec<String> },
    DuplicateInputName { name: String },

    // Interpolation errors — raised only for tokens whose prefix is in
    // the recognized allowlist. Tokens with foreign prefixes pass through
    // without error (they are assumed to belong to downstream tooling
    // such as kubectl/Go-templates, Helm, Jinja).
    UnknownInputReference { location: InterpolationLocation, name: String },
    UnknownWorkflowField { location: InterpolationLocation, name: String },
}

pub struct InterpolationLocation {
    pub task: String,
    pub role: String,
    pub field: InterpolationField,
}

pub enum InterpolationField { Prompt, Command }
```

### Rules

- Task names unique within a workflow.
- Role names unique within a task.
- Every `depends_on` entry resolves to an existing task name.
- No cycles; reported with the cycle path (`[a, b, c, a]`).
- Input names unique.
- `{{...}}` tokens in any prompt or command whose prefix matches the allowlist (`inputs.`, `workflow.`) must resolve. Tokens with any other prefix are passed through verbatim and not validated (see Interpolation).

### Where validation runs

- **Client-side before `kubectl apply`**, via the `flotilla-resources validate <path>` CLI or a future higher-level `flotilla` subcommand.
- **Inside any consumer on read** (Stage 3's convoy controller will re-validate before using a template).
- **Not** via a status-writing controller in this stage. See the deferred list.

## Interpolation

Prompts and commands may contain `{{path}}` tokens that are resolved at convoy launch. Stage 2 only parses them and validates that the ones we recognize resolve.

### Syntax

- A token starts with `{{` and ends with `}}`.
- The path between the braces is a dotted sequence of segments matching `[A-Za-z0-9_-]+`. No internal whitespace in v1 (Argo allows whitespace but documents interpolation bugs around it; stricter is simpler).
- No escape for literal `{{` is needed because foreign tokens pass through unchanged — see below.

### Recognized prefix allowlist (v1)

Only tokens whose first segment is one of these scopes are subject to validation and substitution:

| Prefix | Examples | Meaning |
|--------|----------|---------|
| `inputs.<name>` | `{{inputs.branch}}`, `{{inputs.feature}}` | Declared workflow input; must appear in `spec.inputs[*].name`. |
| `workflow.name` | `{{workflow.name}}` | Convoy name (set at launch). |
| `workflow.namespace` | `{{workflow.namespace}}` | Convoy namespace. |

### Foreign-token passthrough

Tokens whose first segment is **not** in the allowlist are left in place verbatim. The validator does not inspect them; convoy launch does not substitute them. They are assumed to be downstream-tool templates. Examples that pass through unchanged:

- `kubectl get pod -o go-template='{{.metadata.name}}'` (Go template — leading `.`)
- `helm template . --set name={{ .Release.Name }}` (Helm template)
- Jinja, Mustache, or other `{{...}}` based languages embedded in commands.

This matches Argo's approach (`workflow/common/common.go#GlobalVarValidWorkflowVariablePrefix`, `workflow/validate/validate.go#checkValidWorkflowVariablePrefix`).

### Tradeoff

Tokens like `{{inptus.branch}}` (typo of `inputs.`) are treated as foreign and pass through silently. The validator cannot tell a misspelled scope from a real downstream template. This is the accepted cost of natural interop with Go templates, Helm, etc. — Argo makes the same tradeoff.

### Absent values

All declared inputs in v1 are required — convoy launch must supply a value. Optional inputs and the "absent ⇒ empty string" semantics that Argo uses for skipped-task outputs are deferred (see "Optional / multi-valued inputs" in the deferred list).

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

Matches the design doc's YAML shape verbatim and reads naturally when authored by humans or agents. Serde's `#[serde(flatten, untagged, deny_unknown_fields)]` catches structural errors (neither or both sources, unknown fields) at parse time, before the validator runs. The CRD's `oneOf` + `not` block enforces the same rule at the API layer so `kubectl apply` doesn't admit a template that Rust can't read.

### No status, no controller

The brainstorm prompt framed WorkflowTemplate as "pure data, no controller needed." We kept that framing. `validate()` is a pure function over the spec; any consumer can run it on read. Adding a status subresource now without a writer would be misleading; adding a writer would inflate Stage 2 beyond its scope. The "fail fast at apply" UX is better served by a client-side CLI validator than by a status subresource.

### Inputs declared at workflow level

`inputs:` makes it explicit what a convoy must supply at launch. This lets the validator check that `{{inputs.<name>}}` references are resolvable without running the convoy, and documents the template's interface. All v1 inputs are required; optionality and multi-valued inputs are deferred (bundled together because they share a runtime-semantics question — likely Argo-style empty-string substitution for absent values).

### Argo-style `{{...}}` interpolation with prefix allowlist

Collision with real commands (`jq '{name: .name}'`, brace expansion, etc.) ruled out single-brace `{var}`. `{{var}}` matches Argo's well-trodden convention, and scoping from day one (`inputs.<name>`, `workflow.name`) means future additions (`tasks.<name>.outputs.*`, `items.*`, expression form `{{=...}}`) slot in without a migration.

The deeper collision — `{{.metadata.name}}` Go templates, Helm, Jinja — is handled by adopting Argo's **prefix allowlist** model (`workflow/common/common.go#GlobalVarValidWorkflowVariablePrefix`). Only tokens whose first segment is a recognized flotilla scope are validated and substituted; every other token is left alone. No escape character needed; natural interop with downstream template languages. The cost is that typos in our own scope names (`{{inptus.branch}}`) are not caught — an acceptable tradeoff for the interop benefit. Strict no-internal-whitespace in v1 avoids the interpolation bug Argo's own docs flag.

### Parse vs. validate split

Structural errors (missing required fields, wrong types, selector-xor-command violations) are handled by serde at parse time, where the type system makes them unreachable as a running state. The validator runs on already-typed data and covers only the semantic rules that a type system can't express (cycles, name resolution, input references). This keeps each layer's job clear and honest about what it catches.

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
- Optional and multi-valued inputs (starting from 0+ issues, defaults, typed inputs). Runtime semantics likely follow Argo's "absent value ⇒ empty string."
- Additional interpolation scopes (`tasks.<name>.outputs.*`, `items.*`, `workflow.creationTimestamp`, etc.) and the expression form `{{=...}}`.
- Non-terminal content (port-forwarding for dev servers, background services, HTTP probes).
- GitOps sync (templates authored in VCS, synced by an Argo CD / Flux style controller).
- Status subresource + validator controller — likely the right end state once templates reference each other or shared-cluster authoring demands fast-feedback validity.
