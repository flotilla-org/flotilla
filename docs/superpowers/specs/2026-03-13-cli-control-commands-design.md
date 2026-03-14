# CLI Control Commands Design

**Date**: 2026-03-13
**Issue**: #283
**Status**: Approved
**Depends on**: #281, #272
**Related**: #274, #284

## Scope

This design covers the mutating CLI commands deferred from the query-command work:

| Command | Description |
|---------|-------------|
| `flotilla refresh [repo]` | Trigger refresh for one repo, or all tracked repos if omitted |
| `flotilla repo add <path>` | Track a new repo |
| `flotilla repo remove <path_or_slug>` | Stop tracking a repo |
| `flotilla repo <path_or_slug> checkout <branch>` | Materialize a checkout for an existing branch |
| `flotilla repo <path_or_slug> checkout --fresh <branch>` | Create a fresh branch checkout from the default base |
| `flotilla checkout <path_or_branch> remove` | Remove a checkout |

All commands support human-friendly output by default and `--json` for structured output.

## Goals

- Use one shared command protocol for CLI, TUI, and future interfaces such as web and MCP.
- Make host targeting logical rather than transport-specific.
- Keep repo and checkout resolution inside the daemon, not duplicated in frontends.
- Preserve streamed progress for multi-step commands.
- Support remote targeting through the same command model, with peer forwarding handled by the daemon.

## Non-goals

- Explicit checkout destination paths. Existing providers choose the checkout path.
- Provider-specific `create_branch` flags exposed to users or frontends.
- A CLI-only control abstraction separate from the shared protocol.
- Full multi-host query grammar from #284 beyond the control-command host prefix needed here.

## Command Model

The existing `flotilla_protocol::Command` becomes the shared routed command envelope rather than a local repo-scoped payload.

```rust
pub struct Command {
    pub host: Option<HostName>,
    pub action: CommandAction,
}

pub enum CommandAction {
    Checkout {
        repo: RepoSelector,
        target: CheckoutTarget,
    },
    RemoveCheckout {
        checkout: CheckoutSelector,
    },
    Refresh {
        repo: Option<RepoSelector>,
    },
    AddRepo {
        path: PathBuf,
    },
    RemoveRepo {
        repo: RepoSelector,
    },

    // Existing non-CLI actions migrate into this shape over time.
    CreateWorkspaceForCheckout { checkout_path: PathBuf },
    SelectWorkspace { ws_ref: String },
    FetchCheckoutStatus { branch: String, checkout_path: Option<PathBuf>, change_request_id: Option<String> },
    OpenChangeRequest { id: String },
    CloseChangeRequest { id: String },
    OpenIssue { id: String },
    LinkIssuesToChangeRequest { change_request_id: String, issue_ids: Vec<String> },
    ArchiveSession { session_id: String },
    GenerateBranchName { issue_keys: Vec<String> },
    TeleportSession { session_id: String, branch: Option<String>, checkout_key: Option<PathBuf> },
    SetIssueViewport { repo: PathBuf, visible_count: usize },
    FetchMoreIssues { repo: PathBuf, desired_count: usize },
    SearchIssues { repo: PathBuf, query: String },
    ClearIssueSearch { repo: PathBuf },
}
```

### Host targeting

`host: None` means "execute on the daemon I am currently connected to."

`host: Some(name)` means "execute on logical host `name`." The receiving daemon decides whether that host is itself or whether the command must be forwarded over peer routing.

This keeps transport topology out of the protocol. A CLI connected to a remote socket still uses `host: None` for commands meant for that daemon.

### Selectors

Selectors carry exact references when the frontend has them and user queries when it does not:

```rust
pub enum RepoSelector {
    Path(PathBuf),
    Query(String),
}

pub enum CheckoutSelector {
    Path(PathBuf),
    Query(String),
}
```

Why this shape:

- CLI can send the raw user input and let the daemon resolve it centrally.
- TUI and other programmatic frontends can send exact paths when already known.
- Ambiguity and not-found handling become daemon responsibilities with one consistent error model.

### Checkout target

Checkout creation needs two distinct intents:

```rust
pub enum CheckoutTarget {
    Branch(String),
    FreshBranch(String),
}
```

- `Branch(name)` means materialize a checkout for an already-existing branch. It succeeds for an existing local branch or an existing remote-tracking branch and fails if the branch does not exist.
- `FreshBranch(name)` means create a brand new branch checkout from the provider's default base and fail if the branch already exists locally or remotely.

This avoids the race-prone ambiguity of a single "checkout this branch somehow" action while keeping the user-facing verb as `checkout`.

## CLI Grammar

The CLI surface stays close to the existing noun-first structure:

```text
flotilla refresh [repo]
flotilla repo add <path>
flotilla repo remove <path_or_slug>
flotilla repo <path_or_slug> checkout <branch>
flotilla repo <path_or_slug> checkout --fresh <branch>
flotilla checkout <path_or_branch> remove
```

Remote targeting uses a host prefix:

```text
flotilla host feta refresh
flotilla host feta repo add /path/to/repo
flotilla host feta repo owner/repo checkout feature/x
```

The CLI converts these arguments into a routed `Command` and sends it through the daemon. It does not resolve repos, branches, or hosts beyond clap parsing.

## Routing and Execution

### Shared flow

All frontends use the same flow:

1. Construct `Command`
2. Send it to the daemon with `execute(command)`
3. Receive `command_id`
4. Subscribe to `DaemonEvent`
5. Render progress from events
6. Exit or update UI on `CommandFinished`

### Daemon responsibilities

The daemon owns:

- defaulting `host` to the connected daemon when omitted
- deciding local execution vs peer forwarding
- resolving `RepoSelector` and `CheckoutSelector`
- converting routed protocol commands into local executor operations
- emitting lifecycle events and final results

### Local execution

For commands targeting the local daemon:

- `Refresh { repo: None }` refreshes all tracked repos
- `Refresh { repo: Some(selector) }` resolves one repo then refreshes it
- `AddRepo` and `RemoveRepo` remain daemon-level state changes
- `Checkout { repo, target }` resolves the repo, then delegates to the checkout manager using the target semantics
- `RemoveCheckout { checkout }` resolves the checkout to a concrete repo/branch pair, then removes it

The daemon may internally keep optimized paths for simple commands, but those are implementation details behind the shared `execute(command)` contract.

### Remote execution

For commands targeting another host:

- the daemon forwards the same `Command` shape over peer routing
- the target daemon executes it locally
- step updates and completion results are proxied back to the originator

This reuses the routed peer channel model from #272 rather than inventing a separate CLI-only RPC path.

## Internal Boundaries

The protocol command becomes the frontend/daemon boundary, but the daemon still benefits from a cleaner resolved local form.

Recommended split:

- `flotilla-protocol::Command`: routed, unresolved, shared by all frontends
- internal resolved command or execution request: local-only, concrete repo/checkout context after resolution

That internal resolved form is not serialized and is free to stay optimized for daemon execution code.

## Results and Progress

All commands, including simple ones, use the same async lifecycle:

```rust
execute(command) -> command_id
```

Progress and completion arrive through events:

- `CommandStarted`
- `CommandStepUpdate`
- `CommandFinished`

The existing event model remains, but command events should carry enough routing context to be meaningful for remote execution. At minimum:

- `command_id`
- target host
- resolved repo path when applicable
- human-readable description

`CommandResult` should evolve away from "only checkout commands have typed success variants" and cover the new control actions:

```rust
pub enum CommandResult {
    Ok,
    RepoAdded { path: PathBuf },
    RepoRemoved { path: PathBuf },
    Refreshed { repos: Vec<PathBuf> },
    CheckoutCreated { branch: String, path: PathBuf },
    CheckoutRemoved { branch: String },
    BranchNameGenerated { name: String, issue_ids: Vec<(String, String)> },
    CheckoutStatus(CheckoutStatus),
    Error { message: String },
    Cancelled,
}
```

Simple commands may emit only `Started` and `Finished`. Multi-step commands such as checkout creation/removal emit `CommandStepUpdate` events as they execute.

## Resolution Rules

### Repo resolution

Reuse the existing daemon-side repo resolution approach from the query commands:

- exact path match
- exact repo name match
- exact slug match
- unique substring match

Errors:

- no match -> not found
- multiple substring matches -> ambiguous with candidate list

### Checkout resolution

Checkout removal accepts either a path or a query string:

- exact path match against known checkout paths
- exact branch match
- unique substring against branch and checkout path display

Errors:

- no match -> not found
- ambiguous -> candidate list
- matched checkout on a different host than requested -> routing error

## Error Handling

The daemon returns structured command failures rather than frontend-local text decisions.

Important failures:

- target host unknown or unreachable
- repo selector not found
- repo selector ambiguous
- checkout selector not found
- checkout selector ambiguous
- `CheckoutTarget::Branch` used for a branch that does not exist
- `CheckoutTarget::FreshBranch` used for a branch that already exists
- provider missing for checkout operations
- forwarded command timed out or returned an error

Human mode renders concise messages. `--json` returns the structured `CommandResult` payload.

## Testing Strategy

### Protocol

- serde round-trip tests for the evolved `Command`, selectors, and `CheckoutTarget`
- event round-trip tests for any new command-event fields

### Local daemon path

- local `execute(command)` tests for all new control actions
- refresh-all coverage
- repo and checkout resolution coverage
- clear failure tests for ambiguous and not-found selectors

### Socket path

- socket round-trip tests proving CLI-visible commands use the shared execute path
- command lifecycle tests that observe `CommandStarted`, `CommandStepUpdate`, and `CommandFinished`

### Peer routing

- routed command forwarding tests through channel transport
- remote success and remote failure propagation
- reverse-path and disconnect handling for in-flight commands

### CLI

- clap parsing tests for the new grammar
- human output tests for success and error summaries
- `--json` output tests for final result payloads

## Implementation Notes

- Explicit checkout path input is deferred because the current checkout-manager trait does not support it consistently across providers.
- `create_branch` is removed from the shared command intent; provider implementations decide how to realize `CheckoutTarget::Branch` vs `CheckoutTarget::FreshBranch`.
- `host <host>` query commands from #284 remain separate; this issue only needs the host prefix required for control-command routing.
