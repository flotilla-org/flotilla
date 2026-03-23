# Hop Chain Abstraction

**Issue:** #471 (Phase A of #442, partial #368 — terminal pool attach surface only, not cloud agent teleport)
**Date:** 2026-03-23

## Problem

Terminal attach commands are built as strings through two independent, uncoordinated layers: the `TerminalPool` (which builds e.g. `cleat attach 'session' --cwd ...`) and `wrap_remote_attach_commands()` (which wraps that in `ssh -t host '...'`). Each layer does its own shell escaping. Adding a third layer (e.g. `docker exec` for environments) would make the escaping fragile and the code harder to reason about.

The attach command surface is also tightly coupled to specific transports (SSH strings hardcoded in executor code) rather than being transport-agnostic (#368).

## Design

### Core Types

```rust
/// Declarative — what needs to happen, not how
enum Hop {
    RemoteToHost { host: HostName },
    EnterEnvironment { env_id: EnvironmentId, provider: String },  // Phase C
    AttachTerminal { attachable_id: AttachableId },
    RunCommand { command: String },
}

struct HopPlan(Vec<Hop>);
```

```rust
/// Structured command arguments — a tree, not flat strings
enum Arg {
    Bare(String),               // literal flag or subcommand, no quoting needed
    Quoted(String),             // value, quoting applied at flatten time
    NestedCommand(Vec<Arg>),    // subtree rendered as a single quoted argument
}
```

`NestedCommand` means "render this entire subtree into a single shell-quoted argument." It exists because some transports (SSH) pass the inner command as a string argument to a remote shell, while others (Docker exec) pass argv directly. The per-hop resolver decides which to use:

- **SSH wraps via `NestedCommand`:** the inner command becomes a single string argument to SSH, potentially with further nesting for `$SHELL -l -c "..."`:
  ```
  [Bare("ssh"), Bare("-t"), Quoted("user@feta"),
    NestedCommand([Bare("$SHELL"), Bare("-l"), Bare("-c"),
      NestedCommand([Bare("cd"), Quoted("/repo"), Bare("&&"), ...inner...])])]
  ```
- **Docker exec wraps via argv extension:** no `NestedCommand` — inner args are concatenated directly:
  ```
  [Bare("docker"), Bare("exec"), Bare("-it"), Quoted("abc"), ...inner args flat...]
  ```

The resolver implementation knows the right structure for its transport. `flatten()` handles both: `NestedCommand` triggers depth-aware quoting, flat args are quoted at the current depth. The tree is also useful for debug rendering — pretty-print with indentation and color-coding per nesting depth.

```rust
/// Resolved actions — what the consumer actually executes
enum ResolvedAction {
    Command(Vec<Arg>),
    SendKeys { steps: Vec<SendKeyStep> },
}

enum SendKeyStep {
    Type(String),
    WaitForPrompt,
}

struct ResolvedPlan(Vec<ResolvedAction>);
```

### Resolution: Pop, Wrap, Push

Resolution walks the hop plan inside-out. A mutable `ResolutionContext` accumulates a stack of `ResolvedAction`s:

```rust
struct ResolutionContext {
    current_host: HostName,
    current_environment: Option<EnvironmentId>,
    working_directory: Option<PathBuf>,  // remote cwd, used by SSH wrapper for cd prefix
    actions: Vec<ResolvedAction>,
    nesting_depth: usize,
}
```

Each per-hop resolver takes `&mut ResolutionContext` and decides:

- **Wrap:** peek at top of action stack. If it's a `Command`, pop it, combine with own args, push the combined `Command` back. This merges two hops into one action. *How* the combination works depends on the transport — the per-hop resolver knows its own template:
  - SSH: wraps inner args in `NestedCommand` (inner becomes a single shell-string argument): `ssh -t host NestedCommand($SHELL -l -c NestedCommand(cd dir && inner))`
  - Docker exec: concatenates inner args directly (argv extension, no nesting): `docker exec -it container ...inner_args...`
- **SendKeys:** push a new `SendKeys` action. Creates an execution boundary — the consumer must run everything above first, then type into the resulting shell. The resolver knows the "enter" command for its transport:
  - SSH: `ssh -t user@feta` (no command arg, drops into remote shell)
  - Docker exec: `docker exec -it abc bash` (drops into container shell)
  - A subsequent hop that wants to wrap will find a `SendKeys` on top and cannot merge, so it pushes a new `Command` entry instead.
- **Collapse:** current context shows we're already at this point (e.g., `RemoteToHost(feta)` when `context.current_host == feta`). Do nothing.

N hops produce M actions where M <= N. The final stack reads top-to-bottom as execution order.

### Combine Strategy

The choice between wrap and sendkeys at each combination point is made by a `CombineStrategy` injected into the `HopResolver`:

```rust
trait CombineStrategy: Send + Sync {
    fn should_wrap(&self, hop: &Hop, context: &ResolutionContext) -> bool;
}
```

The resolver consults this strategy at each hop before deciding wrap vs sendkeys. Phase A implementations:

- **`AlwaysWrap`** — always nests commands as arguments. Matches current SSH wrapping behavior. Default.
- **`AlwaysSendKeys`** — always creates execution boundaries. Exercises the sendkeys path for testing.

Future strategies (depth-based, per-transport, capability-aware) are additional trait implementations. The plan stays pure data — it declares what needs to happen. The strategy is a runtime decision made during resolution based on current context.

### Per-Hop Resolvers

Each subsystem that owns a hop type provides its resolver:

```rust
trait RemoteHopResolver: Send + Sync {
    fn resolve(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String>;
}

trait TerminalHopResolver: Send + Sync {
    fn resolve(&self, attachable_id: &AttachableId, context: &mut ResolutionContext) -> Result<(), String>;
}

// Phase C:
// trait EnvironmentHopResolver: Send + Sync { ... }
```

**`RemoteHopResolver`** — provided by the transport layer (PeerTransport or transport config). The trait implementation encapsulates all transport-specific knowledge: how to wrap (SSH uses `NestedCommand` with login shell; a future transport might use argv extension or HTTP), how to enter for sendkeys (SSH drops command arg), connection details (multiplex settings, host aliases). Today this knowledge lives in `remote_ssh_info()` and `wrap_remote_attach_commands()` — it migrates here. The `CombineStrategy` tells the resolver *whether* to wrap or sendkeys; the resolver knows *how* for its transport. Future transports (HTTPS, etc.) are alternative implementations of the same trait.

**`TerminalHopResolver`** — uses the pool's new structured method (`attach_args()`) to get an `Arg` tree rather than a pre-escaped string.

### HopPlanBuilder

Constructs a `HopPlan` from an `AttachableId`:

```rust
struct HopPlanBuilder<'a> {
    attachable_store: &'a AttachableStore,
    local_host: &'a HostName,
}

impl HopPlanBuilder<'_> {
    fn build(&self, attachable_id: &AttachableId) -> Result<HopPlan, String>;
}
```

Consults the attachable store for host affinity. If the attachable lives on a different host, prepends `RemoteToHost`. Always ends with `AttachTerminal`. Phase C adds `EnterEnvironment` hops.

### HopResolver

Composes per-hop resolvers and drives the resolution:

```rust
struct HopResolver {
    remote: Arc<dyn RemoteHopResolver>,
    terminal: Arc<dyn TerminalHopResolver>,
    strategy: Arc<dyn CombineStrategy>,
}

impl HopResolver {
    fn resolve(&self, plan: &HopPlan, context: &mut ResolutionContext) -> Result<ResolvedPlan, String>;
}
```

Walks hops inside-out (last hop first), dispatches each to the appropriate per-hop resolver, returns the accumulated `ResolvedPlan`. For example, given `[RemoteToHost(feta), AttachTerminal(att-xyz)]`, resolves `AttachTerminal` first (pushes a `Command`), then `RemoteToHost` pops and wraps it.

### Flatten

A single pure function that converts `Vec<Arg>` to a shell command string:

```rust
fn flatten(args: &[Arg], depth: usize) -> String;
```

Walks the `Arg` tree. `Bare` values pass through. `Quoted` values get shell-quoted appropriate to the current depth. `NestedCommand` recurses at `depth + 1`. This is the only place quoting logic lives.

### TerminalPool Changes

Add a structured method alongside the existing string-returning one:

```rust
trait TerminalPool: Send + Sync {
    // New: returns structured args, no escaping. Sync — no pool needs I/O for arg construction.
    fn attach_args(&self, session_name: &str, command: &str,
                   cwd: &Path, env_vars: &TerminalEnvVars) -> Result<Vec<Arg>, String>;

    // Existing: becomes a default method that flattens
    async fn attach_command(&self, session_name: &str, command: &str,
                           cwd: &Path, env_vars: &TerminalEnvVars) -> Result<String, String> {
        let args = self.attach_args(session_name, command, cwd, env_vars)?;
        Ok(flatten(&args, 0))
    }

    // Other methods unchanged
}
```

Each pool implementation (cleat, shpool, passthrough) adds `attach_args()` returning `[Bare("cleat"), Bare("attach"), Quoted(session_name), ...]`. The existing `attach_command()` stays as a convenience wrapper for any callers that just need a flat string.

### Consumers

**Workspace pane consumer (Phase A):** the step system resolves a `HopPlan` to a `ResolvedPlan`, then flattens `Command` actions to a single string for the workspace pane config. This replaces the current flow through `TerminalManager::attach_command()` + `wrap_remote_attach_commands()`.

**`flotilla attach` CLI (future, #368):** the CLI resolves the hop plan and either execs the flattened command (pure wrapping case) or drives an interactive sequence (sendkeys case). On a host that has flotilla, the resolution can collapse to `flotilla attach <id>` — the remote flotilla resolves remaining hops locally. Same plan, different resolution strategy.

### What Gets Deleted

- `wrap_remote_attach_commands()` in `executor/terminals.rs` — replaced by `RemoteHopResolver`
- `remote_ssh_info()` — knowledge moves to transport layer's resolver
- Manual shell escaping in pool `attach_command()` implementations — replaced by `attach_args()` + `flatten()`
- `escape_for_double_quotes()` usage in attach path — `flatten()` handles depth-aware quoting

## Migration

Each step keeps the system working:

1. Introduce types (`Hop`, `Arg`, `ResolvedAction`, etc.) in new `hop_chain` module
2. Implement `flatten()` with tests for depth-0, depth-1, depth-2 quoting
3. Add `attach_args()` to `TerminalPool` trait — implement for cleat, shpool, passthrough
4. Build `TerminalHopResolver` using `attach_args()`
5. Build `RemoteHopResolver` — extract from `wrap_remote_attach_commands()` and `remote_ssh_info()`
6. Build `HopPlanBuilder` — consults attachable store and host affinity
7. Build `HopResolver` — composes per-hop resolvers, implements pop-wrap-push
8. Wire into `TerminalManager::attach_command()` — use hop chain internally, flatten to string
9. Delete `wrap_remote_attach_commands()` and related code
10. Wire into step system — `CreateWorkspaceFromPreparedTerminal` and `PrepareTerminalForCheckout` use hop chain for remote terminal workspace creation (NOT `ResolveAttachCommand`, which is the cloud-agent teleport path and out of scope)

## Testing

The tree structure makes each layer independently testable:

- **`flatten()` unit tests** — depth-0/1/2 quoting, mixed Bare/Quoted/Nested, edge cases (quotes in values, spaces, special chars)
- **Per-hop resolver tests** — given config, produce expected `Vec<Arg>` tree. Pure functions, no I/O.
- **`HopResolver` tests** — given plan and context, produce expected `ResolvedPlan`. Test collapse, wrapping, sendkeys boundaries, pop-wrap-push mechanics. Run the same plans through `AlwaysWrap` and `AlwaysSendKeys` strategies to verify both paths produce valid output.
- **`HopPlanBuilder` tests** — given attachables and hosts, produce expected `HopPlan`.
- **End-to-end flatten tests** — given an `AttachableId` with known host affinity, final flattened string matches expected output (regression against old `wrap_remote_attach_commands()` behavior).
- **Snapshot tests** — pretty-printed `Arg` trees for common scenarios (local attach, remote attach, future 3-hop).
- **Debug rendering tests** — verify the tree pretty-prints readably for tracing output.

## Open Questions

- **`CloudAgentService::attach_command()`** — teleport commands (`claude --teleport`, `agent --resume`) are a separate attach path that doesn't go through `TerminalPool`. For Phase A these stay as strings — they're single commands with no nesting. Phase C may revisit if environment hops need to wrap agent attach commands.
- **Depth-aware quoting strategy** — should `flatten()` use single quotes at depth 0 and double quotes at depth 1 (matching current behavior), or adopt a uniform strategy? Need to verify compatibility with `ssh` and `docker exec` argument passing.
- **`nesting_depth` on `ResolutionContext`** — used by resolvers to inform strategy decisions (e.g., prefer sendkeys over wrapping at deep nesting) and for debug rendering. Exact thresholds to be determined during implementation.
