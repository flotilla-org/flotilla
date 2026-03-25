# Shared Command Registry — Design Spec

**Issue:** #477
**Date:** 2026-03-24
**Related:** #401 (command palette phase 2), #397 (agent vs session naming), #474 (environment provisioning), #332 (command palette phase 1)

## Goal

A single command vocabulary shared across CLI, TUI command palette, TUI intents, step plans, web, MCP, and shell completions. Commands are the user-visible language of the system — every action flotilla takes is expressible as a command, and higher-level commands decompose into sequences of lower-level ones.

## Command Syntax

```
[target] noun [subject[,subject]*] verb [verb-specific args]
```

- **target** (optional): `host feta`, `env docker-3`. Omitted = inferred from context (env vars in a flotilla terminal, or local host).
- **noun**: what you're acting on — `workspace`, `checkout`, `cr`, `issue`, `agent`, `terminal`, `repo`, `branch`, `host`, `environment`.
- **subject** (optional): the specific instance(s). Can be a comma-separated set (`issue #1,#5,#7`). Type determined by noun. Omitted = inferred from TUI selection or context.
- **verb**: the action to perform on the noun.
- **verb-specific args**: additional arguments, format varies per verb.

**Creation commands** are a special case: the subject doesn't exist yet, so the noun has no subject. Parameters describe what to create, and the command may return a generated ID.

```
checkout create --branch my-feature          # creation — no subject
workspace create --checkout /path/to/co      # creation — no subject
checkout my-feature remove                   # action on existing subject
```

### Examples

```
workspace feat-ws select
checkout my-feature remove
cr #42 close
cr #42 link-issues #1,#5
issue #1,#5,#7 suggest-branch
agent claude-1 teleport
agent claude-1 archive
repo flotilla-org/cleat add
host feta checkout create --branch my-feat   # explicit host target
```

When subjects are inferred:
```
checkout create                              # subject from TUI selection
cr close                                     # subject from TUI selection
```

## Command Registry

The registry is a flat table of command definitions. Each entry specifies:

| Field | Purpose |
|-------|---------|
| noun | Category of thing being acted on |
| verb | Action name |
| description | Human-readable, for help/palette/logging |
| aliases | Alternative names (e.g., `pr` for `cr`) |
| subject_type | What the subject is — branch name, CR id, agent id, etc. |
| subject_cardinality | None (creation), single, optional, set |
| additional_args | Verb-specific argument specs |
| availability | What context is needed (e.g., "requires a checkout", "requires a CR") |
| completion_sources | Per-argument completion source references |
| host_targetable | Whether this command can be prefixed with a host/environment target |
| level | `user` (CLI/palette visible) or `internal` (step-level, not typically invoked directly) |

### Levels

**User commands** appear in CLI help, palette, shell completions. These are the primary vocabulary.

**Internal commands** are the same data structure but not surfaced in help by default. They're the atomic building blocks that user commands decompose into. A user *can* invoke them (for debugging, manual stepping), but they're not the normal entry point.

### Noun Vocabulary

| Noun | Subjects | Provider source |
|------|----------|-----------------|
| `workspace` | workspace ref/name | WorkspaceManager |
| `checkout` | branch name or path | Vcs, CheckoutManager |
| `cr` | CR id (aliases: `pr`) | ChangeRequestTracker |
| `issue` | issue id(s) | IssueTracker |
| `agent` | agent/session id | CloudAgentService (per #397) |
| `terminal` | attachable id | TerminalPool |
| `repo` | repo path or identity | config |
| `branch` | branch name | Vcs |
| `host` | host name | peer mesh |
| `environment` | environment id | EnvironmentProvider (per #474) |
| `attachable-set` | set id | AttachableStore |

## Command Catalog

### User Commands

#### workspace

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `select` | workspace ref | — | — | Switch to existing workspace |
| `create` | (none — creation) | `--checkout <path>` | workspace ref | High-level: decomposes into terminal prepare + workspace create. Local/remote distinction is execution concern, not command concern. See composition patterns below. |

#### checkout

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `create` | (none — creation) | `--branch <name>`, `--fresh` | checkout path | `--fresh` = new branch vs tracking existing. High-level: decomposes into create + link-issues + workspace create. |
| `remove` | branch name | — | — | |
| `status` | branch name | `--checkout-path`, `--cr-id` | checkout status | Query, not mutation. Used by TUI for delete confirmation dialog. |

#### cr (alias: pr)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `open` | CR id | — | — | Opens in browser |
| `close` | CR id | — | — | |
| `link-issues` | CR id | issue ids (positional) | — | Appends "Fixes #N" to PR body |

#### issue

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `open` | issue id | — | — | Opens in browser |
| `suggest-branch` | issue id(s) (set) | — | branch name | AI utility call. Subject is a set. |
| `search` | (none) | query (positional) | — | |

#### agent (currently "session", per #397)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `teleport` | agent id | `--branch`, `--checkout` | — | High-level: decomposes into resolve-attach + ensure-checkout + create-workspace. |
| `archive` | agent id | — | — | |

#### repo

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `add` | repo path | — | — | Track a repository |
| `remove` | repo identity | — | — | Untrack |
| `refresh` | repo identity (optional) | — | — | None = refresh all |

### Internal Commands

These are the atomic building blocks. Currently `StepAction` variants, they become registry entries at `internal` level.

#### checkout (internal verbs)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `ensure-for-teleport` | branch name | `--checkout-key`, `--initial-path` | checkout path | Fast-path if initial-path known. Used in teleport composition. |

#### branch (internal)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `link-issues` | branch name | issue ids | — | Writes issue associations to git config |

#### terminal (internal)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `prepare` | checkout path | template commands | attachable-set id | Allocates attachable set, ensures terminals running, resolves attach args. Currently produces fat `TerminalPrepared` struct — should produce attachable-set handle instead. |

#### attachable-set (internal)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `ensure` | checkout path | — | set id | Allocate or reuse set for checkout |

#### agent (internal verbs)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `resolve-attach` | agent id | — | attach command string | Intermediate result for teleport |
| `create-teleport-workspace` | agent id | `--branch` | — | Consumes resolved attach command + checkout path |

#### workspace (internal verbs)

| Verb | Subject | Additional args | Produces | Notes |
|------|---------|-----------------|----------|-------|
| `create-from-prepared` | (none — creation) | `--checkout <path>`, `--attachable-set <id>` | — | Creates workspace from pre-prepared terminal state. See composition patterns. |

### Daemon-level Commands

Commands that don't require repo context. Handled directly by the daemon.

| Command | Subject | Args | Notes |
|---------|---------|------|-------|
| `repo add` | path | — | Detect and track |
| `repo remove` | identity | — | Untrack |
| `repo refresh` | identity (optional) | — | All repos if omitted |
| `issue search` | (none) | query | Per-repo, inline |
| `issue viewport` | (none) | count | UI-driven, inline. Not a user command — system-level. |

## Composition Patterns

High-level user commands decompose into sequences of lower-level commands. The binding model connects outputs from earlier steps to inputs of later steps.

### Pattern: checkout create

The user says "start working on this branch". This decomposes into:

```
checkout create --branch $branch [--fresh]
  → $checkout_path

branch $branch link-issues $issue_ids          [conditional: if $issue_ids present]

workspace create --checkout $checkout_path
```

Currently `build_create_checkout_plan` in `executor.rs`.

### Pattern: workspace create

The user says "create a workspace for this checkout". This decomposes into terminal preparation and workspace creation. Whether the checkout is local or remote is a routing concern, not a different command.

```
terminal $checkout_path prepare
  → $attachable_set_id

workspace create-from-prepared --checkout $checkout_path --attachable-set $attachable_set_id
```

**Note on TerminalPrepared:** Today `terminal prepare` produces a fat `TerminalPrepared` struct (repo identity, target host, branch, checkout path, attachable set id, resolved pane commands). This should produce just the attachable-set handle. The workspace creation step queries the store for what it needs using that handle. The system may cache the snapshot alongside the handle as an optimization.

**Note on dual-target:** Today there's a split between `CreateWorkspaceForCheckout` (local) and `PrepareTerminalForCheckout → CreateWorkspaceFromPreparedTerminal` (remote, via TUI round-trip). In the registry model this is a single `workspace create` that the executor routes correctly — the TUI round-trip becomes executor-internal.

### Pattern: agent teleport

The user says "connect to this remote agent". This decomposes into:

```
agent $agent_id resolve-attach
  → $attach_command

checkout $branch ensure-for-teleport --checkout-key $checkout_key
  → $checkout_path

agent $agent_id create-teleport-workspace --branch $branch
```

Currently `build_teleport_session_plan` in `executor.rs`. Step 3 currently reaches into `prior` to find results from steps 1 and 2 — the binding model replaces this with named references.

### Pattern: checkout remove (with confirmation)

The TUI intent `RemoveCheckout` actually fires `FetchCheckoutStatus` first to populate a confirmation dialog. The actual removal only happens after user confirmation. This is a UI flow, not a composition:

- `checkout <branch> status` — query, populates dialog
- User confirms
- `checkout <branch> remove` — mutation

The registry models these as two separate commands. The TUI orchestrates the confirm-then-act flow.

### Observations on Composition

Some "commands" in the current code are really pre-composed conveniences (`checkout create` = create + link-issues + workspace). Over time, composition on the fly from primitives may replace these. For now, keeping them as named user commands that expand into plans is the right approach — they represent user intent ("start working on this branch"), not just a sequence of steps.

## Stepper / Plan Executor

The new stepper is a generic engine that replaces the current `StepAction`-specific executor.

**Execution loop:**
1. For each step in the plan:
   - Resolve input bindings from the binding table
   - Skip if conditional and bindings are absent
   - Determine target (host/environment) from command spec and context
   - Execute the command via its registered executor
   - Store outputs in the binding table
   - Log: the command text with resolved bindings IS the log entry

**What it gains:**
- No per-command match arms — dispatch through the registry
- Named bindings replace positional `prior` value threading
- Logging is automatic and human-readable
- Plans are inspectable and serializable (can show in TUI before/during execution)
- Manual stepping becomes possible (pause, inspect bindings, resume)

**Future extensions (not phase 1):**
- `par` — parallel execution of independent steps
- `seq` — explicit sequencing (default)
- `loop` — repeat over a set (e.g., for each terminal in a set)
- User-authored plans

## CLI Generation

The CLI is generated from the registry. Each noun becomes a subcommand group, verbs become subcommands within.

```
flotilla workspace feat-ws select
flotilla checkout create --branch my-feature --fresh
flotilla cr #42 close
flotilla repo flotilla-org/cleat add
flotilla refresh
```

**Phase 1 execution path:** The CLI parses using registry arg specs, produces a `CommandAction` via the command's legacy mapping, wraps in a `Command`, sends to the daemon. Identical to today's execution, entered through the registry instead of hand-written clap.

**Context inference:** Inside a flotilla terminal (`FLOTILLA_ATTACHABLE_ID` set), the CLI infers current repo, host, and environment. `flotilla cr close` without a subject means "close the CR for the current checkout in this terminal's context."

## Shell Completions

Generated from the registry via a hidden subcommand:

```
flotilla complete <line> <cursor-pos>
```

The completion engine walks the parse state:
1. No noun yet → complete with noun names
2. Noun entered → complete with subjects (dynamic, from completion source)
3. Noun + subject → complete with valid verbs
4. Verb entered → complete with verb-specific args

Shell-specific scripts (bash/zsh/fish) are boilerplate that calls `flotilla complete`.

### Completion Sources

```rust
trait CompletionSource: Send + Sync {
    async fn completions(&self, prefix: &str, context: &CompletionContext) -> Vec<CompletionItem>;
}
```

- **Static sources:** verb names, flag names — from registry metadata
- **Dynamic sources:** subjects — query daemon for workspace names, branch names, CR ids, etc.
- **Context-derived:** from TUI selection or env vars

Each noun's subject type maps to a provider-backed completion source.

## Palette Integration

The command palette (Phase 2, #401) consumes the registry:

- Typing a noun shows available verbs
- Typing a subject triggers completion from the noun's completion source
- The selected work item pre-fills subjects
- Selecting a command from the palette resolves and executes it

**Command echo:** When a TUI action is triggered by key binding (e.g., `d` for delete), the palette/status bar briefly shows the resolved command text: `checkout my-feature remove`. Using the TUI teaches the CLI vocabulary.

## Crate Structure

New crate: `flotilla-commands`

| Responsibility | In crate |
|---|---|
| Command definitions (noun, verb, args, description) | Yes |
| Argument types and subject specs | Yes |
| Completion source trait + static sources | Yes |
| Plan composition definitions | Yes (phase 3) |
| Registry (flat table of all commands) | Yes |
| CLI parser generation from registry | Yes |
| Shell completion generation | Yes |
| Legacy mapping to `CommandAction` | Yes (phase 1, removed later) |
| Plan executor / stepper | No — stays in `flotilla-core` |
| Dynamic completion providers | No — stays in `flotilla-core` (provider-backed) |
| Palette rendering | No — stays in `flotilla-tui` |

### Dependency Direction

```
flotilla-protocol (wire types: CommandAction, Command, etc.)
    ↑
flotilla-commands (registry, definitions, CLI, completions)
    ↑
flotilla-core (executor, providers, dynamic completions)
    ↑
flotilla-tui (palette rendering, key bindings)
```

## Phasing

### Phase 1: Registry + CLI

Define the command registry with all current user-visible commands. Generate CLI from registry, replacing hand-written clap. Shell completions from registry. Each command maps to existing `CommandAction` for execution — no stepper changes.

**Deliverables:**
- `flotilla-commands` crate with registry
- CLI generated from registry
- Shell completion via `flotilla complete`
- All current CLI functionality preserved

**Scope note:** Phase 1 maps registry commands down to existing `CommandAction` / `build_plan` / stepper. The registry is a new front door to the same back end. The existing execution machinery is unchanged.

### Phase 2: Palette Integration

Command palette consumes registry for Phase 2 (#401). TUI intents become thin lookups into the registry. UI actions echo resolved commands.

**Deliverables:**
- Palette shows noun-verb commands with completions
- Key bindings resolve through registry
- Command echo in status bar

### Phase 3: Composition + New Stepper

Plans expressed as command sequences with named bindings. New generic stepper replaces `StepAction`-specific executor. `build_plan()` migrated to declarative composition.

**Deliverables:**
- Plan composition model in registry
- New stepper with binding resolution
- `build_plan()` → declarative plans
- Plan inspection/logging in TUI
- `TerminalPrepared` fat struct → attachable-set handle

### Phase 4: Step Dissolution

Step-level commands promoted to first-class registry entries at `internal` level. `StepAction` enum dissolved. All execution goes through the registry.

**Deliverables:**
- `StepAction` removed
- All commands (user and internal) in registry
- Full command-level logging and tracing

## Open Questions

- Exact verb-specific argument syntax (positional vs flags) — to be determined per command during implementation.
- Whether `host` and `environment` are pure target prefixes or also nouns with their own verbs (e.g., `host feta status`). Likely both.
- Interaction model for command echo — timing, duration, palette vs status bar.
- How composition-on-the-fly evolves — whether pre-composed user commands eventually dissolve into CLI-composable primitives (like shell pipelines) or stay as named conveniences.
