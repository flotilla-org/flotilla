# Shared Command Registry Phase 1 — Design Spec

**Issue:** #477
**Date:** 2026-03-24
**Parent spec:** `docs/superpowers/specs/2026-03-24-shared-command-registry-design.md`

## Goal

Create a `flotilla-commands` crate with typed noun-verb command structs using clap derive. Replace the hand-written CLI parsing in `main.rs` with these shared types. Add static shell completions. Preserve all existing CLI functionality and expose new CLI commands for actions currently available only in the TUI.

## Key Design Decisions

### Clap derive as the shared abstraction

The noun structs use clap's `#[derive(Parser)]` / `#[derive(Subcommand)]`. Both CLI and TUI parse through the same types — CLI from `std::env::args()`, TUI from constructed token vectors via `try_parse_from`. No custom arg spec types needed; clap's `Arg`, `ArgMatches`, and value parsers handle typed extraction, validation, help text, and error messages.

This makes clap the command definition layer for Phase 1. If clap becomes limiting in later phases, the noun structs can be replaced with custom types — the resolve functions and `Resolved` enum remain stable.

### Subject-before-verb via clap

Clap 4 supports `repo myslug checkout main` natively with two settings:

- `subcommand_precedence_over_arg(true)` — verb names match before positionals consume them
- `subcommand_negates_reqs(true)` — verb presence waives the required subject

No manual parsing needed. The plan's Option B (manual verb parsing within noun handlers) is unnecessary.

### Two-stage parsing for host routing

`host` is both a noun (with verbs like `list`, `status`) and a routing prefix that embeds other commands (`host feta repo myslug checkout main`). Clap cannot express this nesting in one pass. A `Refinable` trait handles the second stage:

1. Clap parses `HostNounPartial` — host verbs are typed, routed commands are captured as `Vec<OsString>` via `external_subcommand`
2. The `refine()` step re-parses the raw tokens through `NounCommand::try_parse_from`, producing a fully typed `HostNoun`

This pattern extends to `environment` in later phases. Nouns without partial parsing skip the step.

### `--json` as a global flag

Output format is a presentation concern, not a command concern. `--json` becomes a `global = true` flag on the root `Cli` struct. The dispatch layer passes it to output formatters after resolution. Resolve functions never see it.

This eliminates `normalize_cli_args` — no need to reposition `--json` before variadic positionals.

### Display for stringification

All noun structs implement `Display`, producing the canonical command string (`repo myslug checkout main`). This supports logging now and plan execution in Phase 3, where plans are command strings with `$binding` syntax that parse through the same `try_parse_from` path after substitution.

## Type Hierarchy

### Parse → Refine → Resolve → Dispatch

```
argv / TUI tokens
    ↓ parse (clap derive)
NounStruct { subject, verb: VerbEnum { typed args } }
    ↓ refine (where needed — host routing)
NounStruct with fully typed inner command
    ↓ resolve()
Resolved (command or query)
    ↓ dispatch
run_control_command() / run_query()
```

### NounCommand

All domain nouns in one enum, used by host routing and top-level dispatch:

```rust
pub enum NounCommand {
    Repo(RepoNoun),
    Checkout(CheckoutNoun),
    Cr(CrNoun),
    Issue(IssueNoun),
    Agent(AgentNoun),
    Workspace(WorkspaceNoun),
    Host(HostNoun),
}
```

### Resolved

Output of resolve — what `main.rs` dispatches on:

```rust
pub enum Resolved {
    Command(Command),
    RepoDetail { slug: String },
    RepoProviders { slug: String },
    RepoWork { slug: String },
    HostList,
    HostStatus { host: String },
    HostProviders { host: String },
}

impl Resolved {
    pub fn set_host(&mut self, host: String) { /* sets Command.host or query host */ }
}
```

### Refinable

For two-stage parsing:

```rust
pub trait Refinable {
    type Refined;
    fn refine(self) -> Result<Self::Refined, String>;
}
```

Phase 1: only `HostNounPartial` implements this.

## Noun Struct Design

Each noun module exports a struct (clap `Parser`) and verb enum (clap `Subcommand`), plus `resolve(self) -> Result<Resolved, String>` and `Display`.

### repo

```rust
#[derive(Parser)]
#[command(about = "Manage repositories")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct RepoNoun {
    pub subject: Option<String>,
    #[command(subcommand)]
    pub verb: Option<RepoVerb>,
}

#[derive(Subcommand)]
pub enum RepoVerb {
    Add { path: PathBuf },
    Remove { repo: String },
    Refresh { repo: Option<String> },
    Checkout { branch: String, #[arg(long)] fresh: bool },
    PrepareTerminal { path: PathBuf },
    Providers,
    Work,
}
```

Subject + verb resolution:

| Input | Resolves to |
|-------|-------------|
| `repo add /path` | `Command(TrackRepoPath { path })` |
| `repo remove org/repo` | `Command(UntrackRepo { repo })` |
| `repo refresh` | `Command(Refresh { repo: None })` |
| `repo refresh org/repo` | `Command(Refresh { repo: Some(..) })` |
| `repo myslug refresh` | `Command(Refresh { repo: Some(..) })` — subject used as repo |
| `repo myslug` | `RepoDetail { slug }` |
| `repo myslug providers` | `RepoProviders { slug }` |
| `repo myslug work` | `RepoWork { slug }` |
| `repo myslug checkout feat` | `Command(Checkout { repo, target: Branch(..) })` |
| `repo myslug checkout --fresh feat` | `Command(Checkout { repo, target: FreshBranch(..) })` |
| `repo myslug prepare-terminal /path` | `Command(PrepareTerminalForCheckout { .. })` |

### checkout

```rust
#[derive(Parser)]
#[command(about = "Manage checkouts")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct CheckoutNoun {
    pub subject: Option<String>,
    #[command(subcommand)]
    pub verb: Option<CheckoutVerb>,
}

#[derive(Subcommand)]
pub enum CheckoutVerb {
    Create { #[arg(long)] branch: String, #[arg(long)] fresh: bool },
    Remove,
    Status { #[arg(long)] checkout_path: Option<PathBuf>, #[arg(long)] cr_id: Option<String> },
}
```

| Input | Resolves to |
|-------|-------------|
| `checkout create --branch feat` | `Command(Checkout { target: Branch(..) })` |
| `checkout create --branch feat --fresh` | `Command(Checkout { target: FreshBranch(..) })` |
| `checkout my-feature remove` | `Command(RemoveCheckout { .. })` |
| `checkout my-feature status` | `Command(FetchCheckoutStatus { .. })` |

### cr (alias: pr)

```rust
#[derive(Parser)]
#[command(about = "Code review", visible_alias = "pr")]
pub struct CrNoun {
    pub subject: String,
    #[command(subcommand)]
    pub verb: CrVerb,
}

#[derive(Subcommand)]
pub enum CrVerb {
    Open,
    Close,
    LinkIssues { issue_ids: Vec<String> },
}
```

### issue

```rust
#[derive(Parser)]
#[command(about = "Issues")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct IssueNoun {
    /// Issue ID or comma-separated IDs (e.g. "#1,#5,#7")
    pub subject: Option<String>,
    #[command(subcommand)]
    pub verb: Option<IssueVerb>,
}

#[derive(Subcommand)]
pub enum IssueVerb {
    Open,
    SuggestBranch,
    Search { query: String },
}
```

### agent

```rust
#[derive(Parser)]
#[command(about = "Cloud agents")]
pub struct AgentNoun {
    pub subject: String,
    #[command(subcommand)]
    pub verb: AgentVerb,
}

#[derive(Subcommand)]
pub enum AgentVerb {
    Teleport {
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    Archive,
}
```

### workspace

```rust
#[derive(Parser)]
#[command(about = "Workspaces")]
pub struct WorkspaceNoun {
    pub subject: String,
    #[command(subcommand)]
    pub verb: WorkspaceVerb,
}

#[derive(Subcommand)]
pub enum WorkspaceVerb {
    Select,
}
```

`workspace create` is deferred — it requires routable step executor cleanup.

### host

Two types — partial (what clap parses) and refined (fully typed):

```rust
#[derive(Parser)]
#[command(about = "Manage and route to hosts")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct HostNounPartial {
    pub subject: Option<String>,
    #[command(subcommand)]
    pub verb: Option<HostVerbPartial>,
}

#[derive(Subcommand)]
pub enum HostVerbPartial {
    List,
    Status,
    Providers,
    Refresh { repo: Option<String> },
    #[command(external_subcommand)]
    Route(Vec<OsString>),
}
```

After refinement:

```rust
pub struct HostNoun {
    pub subject: Option<String>,
    pub verb: HostVerb,
}

pub enum HostVerb {
    List,
    Status,
    Providers,
    Refresh { repo: Option<String> },
    Route(NounCommand),
}
```

`HostNoun::resolve()` delegates to the inner command's resolve and sets `command.host` via `Resolved::set_host`. For `Command` variants, this sets `Command.host`. For query variants that already carry a host field (`HostStatus`, `HostProviders`), it's a no-op — the host is already populated by `HostNoun::resolve()`. For query variants without a host field (`RepoDetail`, `RepoProviders`, `RepoWork`), `set_host` is not called — these queries are always local. If a host-routed command resolves to a repo query (e.g., `host feta repo myslug providers`), the resolve function produces a `Resolved::Command` that wraps the query as a daemon-routed command rather than a direct query variant.

## Context Injection

Several `CommandAction` variants need `context_repo` on the wrapping `Command`. Where the repo comes from depends on the noun:

| Noun | Repo source |
|------|-------------|
| `repo` | Subject is the repo slug → resolve sets `context_repo` or the action's `repo` field directly |
| `checkout` | `checkout create` infers repo from context; `checkout <branch> remove/status` does not need repo |
| `cr`, `issue`, `agent`, `workspace` | No repo in the noun struct — requires context |

For nouns that need context, the resolve function leaves `context_repo: None`. The dispatch layer in `main.rs` injects it from:

1. A `--repo` flag on the root `Cli` (if specified)
2. The `FLOTILLA_REPO` environment variable (inside a flotilla terminal)
3. CWD-based repo detection (fallback)

If none of these provide a repo, commands that require one fail with a clear error at dispatch time, not at resolve time. This matches the parent spec's "context inference" model.

**Comma-separated subjects:** `issue #1,#5,#7 suggest-branch` parses `subject` as the single string `"#1,#5,#7"`. The resolve function splits on commas to produce the `Vec<String>` needed by `CommandAction::GenerateBranchName { issue_keys }`.

## Integration with main.rs

### Root CLI

```rust
#[derive(Parser)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long)]
    repo_root: Vec<PathBuf>,
    #[arg(long)]
    config_dir: Option<PathBuf>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    embedded: bool,
    #[arg(long)]
    theme: Option<String>,
    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(Subcommand)]
enum SubCommand {
    // Infrastructure — unchanged
    Daemon { #[arg(long, default_value = "300")] timeout: u64 },
    Status,
    Watch,
    Topology,
    Hook { harness: String, event_type: String },
    Hooks { #[command(subcommand)] command: HooksSubCommand },
    Complete { line: String, #[arg(default_value = "0")] cursor_pos: usize },
    Completions { shell: Shell },

    // Domain — from flotilla-commands
    Repo(RepoNoun),
    Checkout(CheckoutNoun),
    Cr(CrNoun),
    Issue(IssueNoun),
    Agent(AgentNoun),
    Workspace(WorkspaceNoun),
    Host(HostNounPartial),
}
```

### Dispatch

```rust
let format = OutputFormat::from_json_flag(cli.json);

match cli.command {
    Some(SubCommand::Daemon { .. }) => run_daemon(&cli, ..).await,
    Some(SubCommand::Status) => run_status(&cli, format).await,
    Some(SubCommand::Watch) => run_watch(&cli, format).await,
    Some(SubCommand::Topology) => run_topology_command(&cli, format).await,
    Some(SubCommand::Hook { .. }) => run_hook(&cli, ..).await,
    Some(SubCommand::Hooks { .. }) => run_hooks_command(..).await,
    Some(SubCommand::Complete { line, cursor_pos }) => run_complete(&line, cursor_pos),
    Some(SubCommand::Completions { shell }) => run_completions(shell),

    Some(SubCommand::Repo(noun)) => dispatch(noun.resolve()?, &cli, format).await,
    Some(SubCommand::Cr(noun)) => dispatch(noun.resolve()?, &cli, format).await,
    Some(SubCommand::Checkout(noun)) => dispatch(noun.resolve()?, &cli, format).await,
    Some(SubCommand::Issue(noun)) => dispatch(noun.resolve()?, &cli, format).await,
    Some(SubCommand::Agent(noun)) => dispatch(noun.resolve()?, &cli, format).await,
    Some(SubCommand::Workspace(noun)) => dispatch(noun.resolve()?, &cli, format).await,
    Some(SubCommand::Host(partial)) => dispatch(partial.refine()?.resolve()?, &cli, format).await,

    None => run_tui(cli).await,
}
```

### What gets deleted

- `normalize_cli_args` and `find_subcommand_index`
- `parse_repo_command`, `parse_host_command`, `parse_host_control_command`
- `RepoCommand`, `RepoQueryCommand`, `HostCommand`, `HostQueryCommand` enums
- Old `Repo`, `Checkout`, `Host`, `Refresh` subcommand variants
- Per-command `json: bool` fields

### What stays

- `Cli` struct (modified — global `--json`, domain nouns added)
- `run_control_command`, `run_status`, `run_watch`, query runners (execution layer)
- Infrastructure subcommand handling
- `connect_daemon`, `run_tui`, `reset_sigpipe`

## Shell Completions

### Problem

Two features break naive completion (e.g. `clap_complete`):

1. `subcommand_precedence_over_arg` — a token could be a subject or a verb
2. `external_subcommands` on host — clap does not see the inner command tree

### Solution

A custom completion engine that walks the clap `Command` tree, with one special case for the host → noun transition:

```rust
pub fn complete(line: &str, cursor_pos: usize) -> Vec<CompletionItem> {
    let tokens = tokenize(&line[..cursor_pos]);
    let root = build_root_command();
    walk_for_completions(&tokens, &root, 0)
}

fn walk_for_completions(tokens: &[&str], cmd: &Command, pos: usize) -> Vec<CompletionItem> {
    if pos >= tokens.len() {
        return valid_next_tokens(cmd);
    }

    let token = tokens[pos];

    // Try matching as subcommand (verb)
    if let Some(sub) = find_subcommand(cmd, token) {
        return walk_for_completions(tokens, sub, pos + 1);
    }

    // Host routing: external_subcommands position → try matching as noun
    if cmd.is_allow_external_subcommands_set() {
        if let Some(noun_cmd) = find_noun_command(token) {
            return walk_for_completions(tokens, &noun_cmd, pos + 1);
        }
    }

    // Positional arg (subject) — consume and continue
    walk_for_completions(tokens, cmd, pos + 1)
}
```

`valid_next_tokens` returns subcommand names (verbs) and flag names from the current command. `find_noun_command` looks up a noun by name and returns its clap `Command` via `NounStruct::command()`.

Phase 1 completions are static (nouns, verbs, flags). Dynamic completions (subjects from daemon queries) plug into `valid_next_tokens` in Phase 2 without changing the engine structure.

### Shell integration

- `flotilla complete <line> <cursor_pos>` — hidden subcommand, outputs tab-separated value + description per line
- `flotilla completions <shell>` — outputs shell-specific boilerplate (bash/zsh/fish) that calls `flotilla complete`. Shell scripts are hardcoded templates per shell, selected by a `Shell` enum (either from `clap_complete` or defined locally in `flotilla-commands`)

## Scope

### Delivers

- `flotilla-commands` crate with typed noun structs (clap derive)
- Resolve functions producing `Resolved` (commands + queries)
- Host routing via `Refinable` two-stage parse
- `Display` on all noun structs for command echo and logging
- Shell completion engine (static, custom tree walker)
- `complete` and `completions` subcommands
- `--json` as global flag
- All existing CLI functionality preserved
- New CLI commands: `workspace select`, `cr open/close/link-issues`, `issue open/suggest-branch/search`, `agent teleport/archive`, `checkout create/status`

### Defers

- **Issue viewport management** (`SetIssueViewport`, `FetchMoreIssues`, `SearchIssues`, `ClearIssueSearch`) — needs design rethink about CLI surface
- **`workspace create`** — needs routable step executor cleanup
- **Dynamic completions** (subjects from daemon queries) — Phase 2
- **TUI palette integration** — Phase 2
- **Plan composition / `Bindable<T>`** — Phase 3

### Deletes from main.rs

- `normalize_cli_args`, `find_subcommand_index`
- `parse_repo_command`, `parse_host_command`, `parse_host_control_command`
- All old domain subcommand variants and intermediate enums
- Per-command `json` fields

## Testing

### Resolve round-trip tests

For each command the old CLI supported, verify the noun struct produces the same `Command`:

```rust
#[test]
fn repo_add_resolves() {
    let noun = RepoNoun::try_parse_from(["repo", "add", "/tmp/test"]).unwrap();
    let resolved = noun.resolve().unwrap();
    assert_eq!(resolved, Resolved::Command(Command {
        host: None, context_repo: None,
        action: CommandAction::TrackRepoPath { path: PathBuf::from("/tmp/test") },
    }));
}
```

### Host refinement tests

Verify two-stage parsing produces correct inner commands with `host` set:

```rust
#[test]
fn host_routes_repo_command() {
    let partial = HostNounPartial::try_parse_from(
        ["host", "feta", "repo", "myslug", "checkout", "main"]
    ).unwrap();
    let noun = partial.refine().unwrap();
    let resolved = noun.resolve().unwrap();
    assert!(matches!(resolved, Resolved::Command(cmd) if cmd.host.is_some()));
}
```

### Display round-trip tests

`parse → display → parse` produces the same result:

```rust
#[test]
fn display_round_trips() {
    let noun = RepoNoun::try_parse_from(["repo", "myslug", "checkout", "main"]).unwrap();
    let text = noun.to_string();
    let reparsed = RepoNoun::try_parse_from(text.split_whitespace()).unwrap();
    assert_eq!(noun, reparsed);
}
```

### Completion engine tests

- Empty input → all nouns + infrastructure commands
- `repo` → repo verbs (add, remove, refresh, checkout, ...)
- `repo myslug` → repo verbs (same list — subject consumed)
- `host feta` → host verbs + noun names
- `host feta repo myslug` → repo verbs
- `cr #42 clo` → completes to `close`

### Existing tests

Migrate existing `main.rs` CLI tests. Delete `normalize_cli_args` tests.

## File Map

| File | Action |
|------|--------|
| `Cargo.toml` (workspace root) | Modify — add `flotilla-commands` to workspace members + root dependencies |
| `crates/flotilla-commands/Cargo.toml` | Create |
| `crates/flotilla-commands/src/lib.rs` | Create — crate root, re-exports |
| `crates/flotilla-commands/src/resolved.rs` | Create — `Resolved` enum, `Refinable` trait |
| `crates/flotilla-commands/src/noun.rs` | Create — `NounCommand` enum |
| `crates/flotilla-commands/src/commands/mod.rs` | Create — module root |
| `crates/flotilla-commands/src/commands/repo.rs` | Create — `RepoNoun`, `RepoVerb`, resolve, display |
| `crates/flotilla-commands/src/commands/checkout.rs` | Create — `CheckoutNoun`, `CheckoutVerb` |
| `crates/flotilla-commands/src/commands/cr.rs` | Create — `CrNoun`, `CrVerb` |
| `crates/flotilla-commands/src/commands/issue.rs` | Create — `IssueNoun`, `IssueVerb` |
| `crates/flotilla-commands/src/commands/agent.rs` | Create — `AgentNoun`, `AgentVerb` |
| `crates/flotilla-commands/src/commands/workspace.rs` | Create — `WorkspaceNoun`, `WorkspaceVerb` |
| `crates/flotilla-commands/src/commands/host.rs` | Create — `HostNounPartial`, `HostNoun`, refine |
| `crates/flotilla-commands/src/complete.rs` | Create — completion engine |
| `src/main.rs` | Modify — replace old domain subcommands with noun types, global `--json`, new dispatch |

## Open Questions

- Exact `issue search` semantics — does it search within a specific repo (needing `context_repo`) or across all tracked repos? The current `CommandAction::SearchIssues` requires a `repo` field.
- Whether `host status` with no subject (no host name) should return local host status or be an error.
- Whether `repo myslug refresh` (subject form) and `repo refresh myslug` (verb arg form) should both be supported. The `subcommand_precedence_over_arg` pattern supports the former automatically; the resolve function merges subject with verb arg, preferring the explicit verb arg if both are present.
