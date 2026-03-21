# All-Symbolic Step Execution

Addresses [#405](https://github.com/flotilla-org/flotilla/issues/405) (align step closure signature with resolver) and broadens the scope: eliminate closures and the immediate execution path entirely, leaving one uniform execution model where every command becomes a symbolic step plan.

## Problem

The executor has three execution paths:

1. **Immediate** — `build_plan()` delegates to `execute()`, which runs the action inline and wraps the result in `ExecutionPlan::Immediate`. The `execute()` function handles 14 `CommandAction` variants; 5 of those also have step-plan paths in `build_plan()`, leaving ~9 that are immediate-only. These cannot be individually remoted to another host.
2. **Steps with closures** — Multi-step plans whose steps are `StepAction::Closure(Box<dyn FnOnce(Vec<StepOutcome>) -> StepFuture + Send>)`. Closures are opaque: they cannot be serialized, cannot cross a network boundary, and force captured state to be `'static`. The teleport plan uses `Arc<Mutex<Option<T>>>` slots to pass data between closure steps — a workaround for the fact that closures ignore the prior-outcomes parameter.
3. **Steps with symbolic actions** — One variant today (`CreateWorkspaceForCheckout`), resolved at runtime by `ExecutorStepResolver` with access to `&[StepOutcome]`. Serializable, remotable, clean.

Three paths means three ways for things to work, three sets of assumptions about what data is available and how it flows, and a hard boundary between what can and cannot be remoted at the step level.

## Key Decisions

- **One path.** Every command produces a `StepPlan`. `ExecutionPlan::Immediate` and `execute()` go away. Trivial commands become single-step plans — the overhead of step machinery (Vec allocation, one progress event, one cancellation check) is negligible, and uniform routing comes free.
- **All symbolic.** `StepAction::Closure` is removed. Every step action is a data enum variant resolved by `ExecutorStepResolver`. This makes all steps serializable for future step-level remote routing.
- **Rename `CommandResult` to `CommandValue`.** The type carries both client-facing results and inter-step data. The old name implied a final answer; the new name reflects broader use. The type stays in `flotilla-protocol` where `CommandResult` lives today; the rename is a mechanical search-replace across all crates.
- **`Produced` vs `CompletedWith` encodes binding affinity.** A step returns `CompletedWith(v)` when the value should feed the final result, or `Produced(v)` when it is inter-step data only. Both are visible in `prior` outcomes. This avoids putting explicit bindings in the plan structure — the variant identity serves as the lookup key. This has a known limitation: the value's variant conflates data shape with source identity. A future named-bindings system (`name <- step, res <- step2(name)`) would separate those concerns. The current scheme works while plans remain short.
- **Two new `CommandValue` variants.** `AttachCommandResolved` and `CheckoutPathResolved` carry inter-step data for the teleport plan. No other new variants are needed — existing variants already cover every other step output.

## Updated Types

### StepOutcome

```rust
pub enum StepOutcome {
    /// Step completed, no data to report.
    Completed,
    /// Step produced a value that should become the final CommandValue
    /// (last CompletedWith wins). Also visible to later steps via prior.
    CompletedWith(CommandValue),
    /// Step produced inter-step data. Visible to later steps via prior
    /// but does not contribute to the final result.
    Produced(CommandValue),
    /// Step determined its work was already done.
    Skipped,
}
```

### CommandValue (renamed from CommandResult)

```rust
pub enum CommandValue {
    // --- Existing variants (renamed from CommandResult) ---
    Ok,
    RepoTracked { path: PathBuf, resolved_from: Option<PathBuf> },
    RepoUntracked { path: PathBuf },
    Refreshed { repos: Vec<PathBuf> },
    CheckoutCreated { branch: String, path: PathBuf },
    CheckoutRemoved { branch: String },
    TerminalPrepared {
        repo_identity: RepoIdentity,
        target_host: HostName,
        branch: String,
        checkout_path: PathBuf,
        attachable_set_id: Option<AttachableSetId>,
        commands: Vec<PreparedTerminalCommand>,
    },
    BranchNameGenerated { name: String, issue_ids: Vec<(String, String)> },
    CheckoutStatus(CheckoutStatus),
    Error { message: String },
    Cancelled,

    // --- New inter-step variants ---
    AttachCommandResolved { command: String },
    CheckoutPathResolved { path: PathBuf },
}
```

### StepAction (all symbolic)

Grouped by domain. Each variant carries only action-specific data; the resolver provides infrastructure (registry, providers_data, runner, config, attachable_store, etc.).

```rust
pub enum StepAction {
    // Checkout lifecycle
    CreateCheckout { branch: String, create_branch: bool, intent: CheckoutIntent,
                     issue_ids: Vec<(String, String)> },
    LinkIssuesToBranch { branch: String, issue_ids: Vec<(String, String)> },
    RemoveCheckout { branch: String, terminal_keys: Vec<ManagedTerminalId>,
                     deleted_checkout_paths: Vec<HostPath> },
    FetchCheckoutStatus { branch: String, checkout_path: Option<PathBuf>,
                          change_request_id: Option<String> },

    // Workspace lifecycle
    CreateWorkspaceForCheckout { label: String },
    CreateWorkspaceFromPreparedTerminal { target_host: HostName, branch: String,
                                          checkout_path: PathBuf,
                                          attachable_set_id: Option<AttachableSetId>,
                                          commands: Vec<PreparedTerminalCommand> },
    SelectWorkspace { ws_ref: String },
    PrepareTerminalForCheckout { checkout_path: PathBuf, commands: Vec<PreparedTerminalCommand> },

    // Teleport (session → workspace)
    ResolveAttachCommand { session_id: String },
    EnsureCheckoutForTeleport { branch: Option<String>, checkout_key: Option<PathBuf>,
                                initial_path: Option<PathBuf> },
    CreateTeleportWorkspace { session_id: String, branch: Option<String> },

    // Session operations
    ArchiveSession { session_id: String },
    GenerateBranchName { issue_keys: Vec<String> },

    // External interactions
    OpenChangeRequest { id: String },
    CloseChangeRequest { id: String },
    OpenIssue { id: String },
    LinkIssuesToChangeRequest { change_request_id: String, issue_ids: Vec<String> },

    // Checkout with AlwaysCreate/Inline policy (batch 2).
    // The step-plan `CreateCheckout` uses ReuseKnownCheckout/Deferred policy for
    // the interactive flow. This variant covers the forwarded-command path where
    // the remote daemon should always create and link issues inline.
    CheckoutImmediate { target: CheckoutTarget, issue_ids: Vec<(String, String)> },
}
```

### ExecutorStepResolver (expanded)

The resolver gains fields for everything `execute()` currently receives:

```rust
pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub providers_data: Arc<ProviderData>,
    pub runner: Arc<dyn CommandRunner>,
    pub config_base: PathBuf,
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<PathBuf>,
    pub local_host: HostName,
}
```

Each `StepAction` variant maps to a `resolve()` match arm that delegates to a standalone function. The `resolve()` method is pure dispatch — a routing table from action variant to function call. Behavior lives in the called functions, which are independently testable and receive only the data they need.

```rust
// resolve() is mechanical dispatch:
StepAction::ResolveAttachCommand { session_id } =>
    resolve_attach_command(&self.context(), &session_id, prior).await,
StepAction::CreateCheckout { branch, .. } =>
    create_checkout(&self.context(), &branch, create_branch, intent, &issue_ids).await,
```

This separation means the dispatch layer could later be generated by a macro or become an interpreter-level concern (routing named actions to named functions with typed bindings), without changing the behavior functions themselves.

## Data Flow Examples

### CreateCheckout (3 steps)

```
Step 1: CreateCheckout { branch: "feat/x", create_branch: true, ... }
  → CompletedWith(CheckoutCreated { branch: "feat/x", path: "/repo/wt-feat-x" })

Step 2: LinkIssuesToBranch { branch: "feat/x", issue_ids: [...] }
  → Completed

Step 3: CreateWorkspaceForCheckout { label: "feat/x" }
  resolver reads CheckoutCreated.path from prior
  → Completed
```

Final result: `CheckoutCreated { branch: "feat/x", path: "/repo/wt-feat-x" }` (from step 1's `CompletedWith`).

### TeleportSession (3 steps)

```
Step 1: ResolveAttachCommand { session_id: "ses-123" }
  → Produced(AttachCommandResolved { command: "claude --session ses-123" })

Step 2: EnsureCheckoutForTeleport { branch: Some("feat/x"), checkout_key: None, initial_path: None }
  → Produced(CheckoutPathResolved { path: "/repo/wt-feat-x" })

Step 3: CreateTeleportWorkspace { session_id: "ses-123", branch: Some("feat/x") }
  resolver reads AttachCommandResolved.command + CheckoutPathResolved.path from prior
  → Completed
```

Final result: `CommandValue::Ok` (no step produced a `CompletedWith`).

### Single-step commands (e.g., OpenChangeRequest)

```
Step 1: OpenChangeRequest { id: "123" }
  → Completed
```

Final result: `CommandValue::Ok`.

## What Goes Away

| Removed | Replacement |
|---------|-------------|
| `ExecutionPlan::Immediate` | Every command returns `StepPlan` |
| `execute()` function | Logic moves to `ExecutorStepResolver::resolve()` match arms |
| `StepAction::Closure` variant | Symbolic variants for each action |
| `Arc<Mutex<Option<T>>>` slots in teleport plan | `Produced(CommandValue)` in prior outcomes |
| `Vec<StepOutcome>` clone in closure call | `&[StepOutcome]` borrow in resolver (issue #405) |

## run_step_plan Changes

The step runner simplifies. Today it branches on `Closure` vs symbolic:

```rust
let outcome = match step.action {
    StepAction::Closure(f) => f(outcomes.clone()).await,
    symbolic => resolver.resolve(&step.description, symbolic, &outcomes).await,
};
```

After: every step goes through the resolver. The `outcomes.clone()` disappears. The resolver always receives `&[StepOutcome]`.

```rust
let outcome = resolver.resolve(&step.description, step.action, &outcomes).await;
```

The resolver is no longer optional — `run_step_plan` requires it. The `resolver: Option<&dyn StepResolver>` parameter becomes `resolver: &dyn StepResolver`.

The final-result extraction logic is unchanged: it already scans for the last `CompletedWith` variant. The new `Produced` variant is naturally excluded because only `CompletedWith` feeds the final result.

## Batching

### Batch 1: Eliminate closures (this branch)

Convert all existing closure steps to symbolic `StepAction` variants:
- Teleport steps 1-3 (kills Arc tricks)
- CreateCheckout steps 1-2
- RemoveCheckout step
- ArchiveSession step
- GenerateBranchName step

Add `Produced` variant to `StepOutcome`. Add `AttachCommandResolved` and `CheckoutPathResolved` to `CommandValue`. Remove `StepAction::Closure`. Make resolver mandatory in `run_step_plan`. Update tests.

`ExecutionPlan::Immediate` and `execute()` remain for now — the ~9 immediate-only command actions still route through them.

### Batch 2: Eliminate Immediate (follow-up)

Convert all remaining `execute()` handlers to symbolic step actions. Every `build_plan()` arm returns a `StepPlan`. Remove `ExecutionPlan::Immediate` and `execute()`. `StepAction` could move to `flotilla-protocol` at this point (all variants are serializable, enabling wire transport for step-level remote routing).

### Future: Named bindings

Replace `&[StepOutcome]` with a `StepEnvironment` that supports named lookups. Steps declare output keys in the plan. The resolver reads prior values by name rather than searching by variant. Separates data shape from source identity. Only needed when plans grow complex enough that positional variant-matching becomes confusing.

## Known Limitations

- **Variant-as-identity.** `CommandValue` variants serve as both data containers and lookup keys for inter-step data. Two steps producing the same variant type would collide. Named bindings resolve this but are deferred.
- **`providers_data` is a snapshot.** The resolver holds `providers_data` captured at plan-build time. If a step modifies state (e.g., creates a checkout), later steps see the stale snapshot. This matches today's closure behavior — closures capture the same snapshot — but becomes more visible with symbolic actions. For remote routing where staleness windows are longer, the resolver may need to re-fetch provider data between steps.
- **Some step actions carry pre-resolved data.** `RemoveCheckout` carries a pre-resolved branch name and `deleted_checkout_paths` computed from `providers_data` at `build_plan` time. This means the step is not fully self-contained for remote execution — a remote host would need to resolve these itself. This is a batch 1 concession; batch 2 or the remote-routing work can move resolution into the resolver by carrying a `CheckoutSelector` instead of pre-resolved fields.
- **No parallel steps.** Execution remains sequential. The design does not prevent future parallelism but does not enable it either.
- **Resolver size.** `ExecutorStepResolver::resolve()` will grow to ~17 match arms after batch 2. If this becomes unwieldy, the arms can delegate to domain-specific sub-resolvers.
