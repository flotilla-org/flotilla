# Environment Provisioning Phase E — User-Facing Integration

**Issue:** [#530](https://github.com/flotilla-org/flotilla/issues/530)
**Parent:** [#442](https://github.com/flotilla-org/flotilla/issues/442)
**Depends on:** Phase D (#474) — programmatic environment provisioning E2E

## Goal

Make environment provisioning user-facing. A user selects a provisioning target (bare host, new Docker environment, or existing environment), presses `n` to create a checkout, and the system provisions the environment, clones the repo inside it, creates terminals, and attaches the workspace — all through the existing step plan machinery.

Phase D wired the infrastructure end-to-end. This phase connects the TUI to it. CLI integration follows separately.

## Scope

This spec covers the minimal TUI E2E:

1. Hosts advertise environment provisioning capabilities
2. `ProvisioningTarget` type replaces ad-hoc `host` + `environment` fields
3. TUI lets the user select a provisioning target via the command palette
4. Pressing `n` creates a checkout in the selected target
5. `.flotilla/environment.yaml` provides the environment spec per repo

Out of scope: CLI integration (part 2), environment reuse logic, per-environment socket isolation, terminal set disambiguation, environment lifecycle management (GC, idle timeout).

## ProvisioningTarget

New type in `flotilla-protocol`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvisioningTarget {
    /// Run directly on the host's ambient environment.
    Host(HostName),
    /// Provision a fresh environment on the host using the named provider.
    NewEnvironment { host: HostName, provider: String },
    /// Reuse an existing running environment.
    ExistingEnvironment { host: HostName, env_id: EnvironmentId },
}
```

Every variant carries a `HostName`. A helper `host(&self) -> &HostName` extracts it for routing.

### Display syntax

The provisioning target uses a compact string syntax in the command palette and status bar:

| Target | Syntax | Example |
|--------|--------|---------|
| Bare host | `@<host>` | `@feta`, `@local` |
| New environment | `+<provider>@<host>` | `+docker@feta` |
| Existing environment | `=<env_id>@<host>` | `=env-abc@feta` |

The `@` prefix means "at this host" (ambient). `+` means "create new." `=` means "attach to existing."

Future shorthand (not implemented now): `+docker` (pick host for me), `+` (pick everything), bare `docker@feta` (match existing first, fall back to new).

### Parsing and display

`ProvisioningTarget` implements `FromStr` and `Display` using this syntax. The parser accepts the forms above; display produces them. Invalid strings return an error.

## Command struct changes

Add `provisioning_target` alongside the existing `host` field:

```rust
pub struct Command {
    pub host: Option<HostName>,
    pub provisioning_target: Option<ProvisioningTarget>,
    pub context_repo: Option<RepoSelector>,
    pub action: CommandAction,
}
```

`host` remains the routing hint for daemon dispatch (which peer handles this command). `provisioning_target` carries the full provisioning intent for commands that create checkouts or workspaces. For provisioning commands, `host` is derived from the target's `HostName`.

Commands that don't involve provisioning (queries, issue operations, refresh) leave `provisioning_target` as `None`.

The existing `environment: Option<EnvironmentSpec>` field is removed. The `EnvironmentSpec` is no longer carried on the command — it is read from the repo during step execution.

## Host capability advertising

`provider_names_from_registry()` in `model.rs` currently skips `environment_providers`. Add it:

```rust
collect_names(&mut names, &registry.environment_providers);
```

This causes Docker (and future environment providers) to appear in `HostSummary.providers` as `HostProviderStatus { category: "environment", name: "Docker", healthy: true }`.

The TUI uses this to determine which hosts can offer `NewEnvironment` targets in the command palette completions.

## TUI state changes

### UiState

Replace `target_host: Option<HostName>` with:

```rust
pub provisioning_target: ProvisioningTarget,
```

Not optional — defaults to `ProvisioningTarget::Host(local_hostname)`. "Local bare host" is the default, not a special case.

### WidgetContext

Replace `target_host: Option<&'a HostName>` with:

```rust
pub provisioning_target: &'a ProvisioningTarget,
```

### targeted_command()

`App::targeted_command()` sets both fields on the command:

```rust
fn targeted_command(&self, action: CommandAction) -> Command {
    let target = &self.ui.provisioning_target;
    Command {
        host: Some(target.host().clone()),
        provisioning_target: Some(target.clone()),
        context_repo: None,
        action,
    }
}
```

### Command palette target command

The existing `target` palette entry builds completions from host summaries:

For each known host (local + connected peers):
- Always offer `@<host>` (bare host)
- If the host has `HostProviderStatus { category: "environment", .. }`, offer `+<name>@<host>` for each environment provider
- If the host has entries in `HostSummary.environments`, offer `=<env_id>@<host>` for each running environment

Selecting an entry parses it via `ProvisioningTarget::from_str` and sets `ui.provisioning_target`.

### Status bar

The status bar displays `provisioning_target.to_string()` where it currently shows `@local` or `@<host>`.

### Branch input widget

The popup title includes the provisioning target: `" New Branch +docker@feta "` instead of `" New Branch "`.

The widget still emits the same `CommandAction::Checkout`. The provisioning target flows through `targeted_command()` onto the `Command` envelope. The widget does not need to know whether the target is a bare host or an environment.

## Environment spec: `.flotilla/environment.yaml`

Declarative environment specification per repo, stored at `.flotilla/environment.yaml`:

```yaml
image:
  dockerfile: .flotilla/Dockerfile
token_env_vars:
  - GITHUB_TOKEN
  - ANTHROPIC_API_KEY
```

Or with a registry image:

```yaml
image:
  registry: ubuntu:24.04
token_env_vars: []
```

### Protocol type change

Rename `token_requirements` to `token_env_vars` on `EnvironmentSpec` to reflect the current semantics (literal env var names to forward, not abstract requirement names):

```rust
pub struct EnvironmentSpec {
    pub image: ImageSource,
    pub token_env_vars: Vec<String>,
}
```

### Reading the spec

The spec is read from the repo's default branch on the target host via `git show HEAD:.flotilla/environment.yaml`. This runs as a step (see below), not during discovery.

Reading from the default branch means the spec applies regardless of which branch the user is checking out. Future work may read from the branch being checked out, or allow user overrides.

### Token resolution

The `CreateEnvironment` step resolver reads each env var name from the host's process environment and passes the `(name, value)` pairs into `CreateOpts.tokens`. Missing env vars are skipped with a warning (the environment may still work without them, or the user may not need that service).

## Step plan changes

### New step: ReadEnvironmentSpec

A new `StepAction` variant:

```rust
StepAction::ReadEnvironmentSpec
```

Runs on `StepExecutionContext::Host(target_host)`. Reads `.flotilla/environment.yaml` from the repo root via `git show HEAD:.flotilla/environment.yaml`, deserializes it, and produces:

```rust
StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec: EnvironmentSpec })
```

New `CommandValue` variant: `EnvironmentSpecRead { spec: EnvironmentSpec }`.

### Environment checkout plan

When `build_plan()` receives a `Checkout` command with `provisioning_target: Some(NewEnvironment { host, provider })`, it produces the 7-step plan:

1. **ReadEnvironmentSpec** on `Host(target_host)` — read `.flotilla/environment.yaml`
2. **EnsureEnvironmentImage** on `Host(target_host)` — build/pull image from spec
3. **CreateEnvironment** on `Host(target_host)` — provision container, resolve tokens from host env
4. **DiscoverEnvironmentProviders** on `Host(target_host)` — probe inside container
5. **CreateCheckout** on `Environment(target_host, env_id)` — clone repo inside container
6. **PrepareWorkspace** on `Environment(target_host, env_id)` — set up terminals
7. **AttachWorkspace** on `Host(local_host)` — present workspace

Steps 2-7 already exist. Step 1 is new.

`EnsureEnvironmentImage` reads the `EnvironmentSpec` from step 1's outcome in `prior`. `CreateEnvironment` reads the token env var names from the same spec and resolves them from the host's environment.

### Bare host checkout plan

When `provisioning_target` is `Host(_)` or `None`, `build_plan()` produces the existing 3-step plan (CreateCheckout → LinkIssues → PrepareWorkspace → AttachWorkspace). No change.

### ExistingEnvironment checkout plan

When `provisioning_target` is `ExistingEnvironment { host, env_id }`, skip steps 1-3 (the environment already exists). The plan becomes:

4. **DiscoverEnvironmentProviders** on `Host(target_host)` — may be cached, but re-probe for safety
5. **CreateCheckout** on `Environment(target_host, env_id)`
6. **PrepareWorkspace** on `Environment(target_host, env_id)`
7. **AttachWorkspace** on `Host(local_host)`

## Preconditions

The target host must already have the repo checked out. The Docker container bind-mounts the repo's `.git` directory as `/ref/repo` for `git clone --reference`. If the repo does not exist on the target host, `ReadEnvironmentSpec` (or `resolve_reference_repo`) will fail with a clear error.

Auto-provisioning the repo on a remote host is a separate concern, not addressed here.

## Relation to other issues

- **#465 (Provider/Service split):** The `Command` envelope's `host` + `provisioning_target` + `context_repo` union-of-all-fields shape is interim. #465 will introduce service-specific command types. `provisioning_target` would move onto the commands that need it rather than living on every command.
- **#530 remaining items:** Per-environment socket isolation, terminal set disambiguation, environment reuse logic, environment lifecycle (GC, idle timeout), and multi-repo environment support are tracked on #530 but not in this spec.
- **#442 open questions:** ConfigStore projection, image caching, DirectHost-as-Environment unification, and Proxmox/LXC are future phases.
