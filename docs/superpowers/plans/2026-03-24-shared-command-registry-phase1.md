# Shared Command Registry Phase 1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create a `flotilla-commands` crate with clap-derive noun-verb structs that replace hand-written CLI parsing in `main.rs`, add static shell completions, and expose new CLI commands for TUI-only actions.

**Architecture:** Per-noun clap derive structs parse into typed values, resolve to a `Resolved` enum (daemon commands or queries), and dispatch through a single `dispatch()` function. Host routing uses a two-stage parse via a `Refinable` trait. `--json` is a global flag. A custom completion engine walks the clap `Command` tree.

**Tech Stack:** Rust, clap 4 (derive + builder for introspection), flotilla-protocol types

**Spec:** `docs/superpowers/specs/2026-03-24-shared-command-registry-phase1-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `Cargo.toml` (workspace root) | Modify | Add `flotilla-commands` to workspace members + root dependencies |
| `crates/flotilla-commands/Cargo.toml` | Create | Crate manifest |
| `crates/flotilla-commands/src/lib.rs` | Create | Crate root — re-exports |
| `crates/flotilla-commands/src/resolved.rs` | Create | `Resolved` enum, `Refinable` trait |
| `crates/flotilla-commands/src/noun.rs` | Create | `NounCommand` enum, resolve dispatch |
| `crates/flotilla-commands/src/commands/mod.rs` | Create | Module root for noun definitions |
| `crates/flotilla-commands/src/commands/repo.rs` | Create | `RepoNoun`, `RepoVerb`, resolve, Display |
| `crates/flotilla-commands/src/commands/checkout.rs` | Create | `CheckoutNoun`, `CheckoutVerb`, resolve, Display |
| `crates/flotilla-commands/src/commands/cr.rs` | Create | `CrNoun`, `CrVerb`, resolve, Display |
| `crates/flotilla-commands/src/commands/issue.rs` | Create | `IssueNoun`, `IssueVerb`, resolve, Display |
| `crates/flotilla-commands/src/commands/agent.rs` | Create | `AgentNoun`, `AgentVerb`, resolve, Display |
| `crates/flotilla-commands/src/commands/workspace.rs` | Create | `WorkspaceNoun`, `WorkspaceVerb`, resolve, Display |
| `crates/flotilla-commands/src/commands/host.rs` | Create | `HostNounPartial`, `HostNoun`, `HostVerb`, refine, resolve, Display |
| `crates/flotilla-commands/src/complete.rs` | Create | Completion engine (tree walker) |
| `src/main.rs` | Modify | Replace old domain subcommands with noun types, global `--json`, new dispatch |

---

### Task 1: Create the flotilla-commands crate with Resolved and Refinable

**Files:**
- Create: `crates/flotilla-commands/Cargo.toml`
- Create: `crates/flotilla-commands/src/lib.rs`
- Create: `crates/flotilla-commands/src/resolved.rs`
- Modify: `Cargo.toml` (workspace root, lines 2-7 for members, lines 33-44 for dependencies)

- [ ] **Step 1: Create the crate manifest**

`crates/flotilla-commands/Cargo.toml`:
```toml
[package]
name = "flotilla-commands"
version = "0.1.0"
edition = "2021"
license.workspace = true

[dependencies]
flotilla-protocol = { path = "../flotilla-protocol" }
clap = { version = "4", features = ["derive", "string"] }
```

- [ ] **Step 2: Add to workspace and root dependencies**

In `Cargo.toml` (workspace root), add `"crates/flotilla-commands"` to the `members` list (after line 6). In the `[dependencies]` section (around line 33), add:
```toml
flotilla-commands = { path = "crates/flotilla-commands" }
```

- [ ] **Step 3: Create resolved.rs with Resolved enum and Refinable trait**

`crates/flotilla-commands/src/resolved.rs`:
```rust
use flotilla_protocol::{Command, HostName};

/// Output of noun resolution — what main.rs dispatches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A command to send to the daemon for execution.
    Command(Command),
    /// Query: show repo details.
    RepoDetail { slug: String },
    /// Query: show repo providers.
    RepoProviders { slug: String },
    /// Query: show repo work items.
    RepoWork { slug: String },
    /// Query: list all known hosts.
    HostList,
    /// Query: show host status.
    HostStatus { host: String },
    /// Query: show host providers.
    HostProviders { host: String },
}

impl Resolved {
    /// Set the target host on a resolved command or query.
    /// For Command variants, sets Command.host.
    /// For query variants that carry a host field, this is a no-op
    /// (the host is already populated by the noun's resolve).
    pub fn set_host(&mut self, host: String) {
        match self {
            Resolved::Command(cmd) => {
                cmd.host = Some(HostName::new(&host));
            }
            // Query variants with host are already populated
            Resolved::HostStatus { .. } | Resolved::HostProviders { .. } | Resolved::HostList => {}
            // Repo queries routed through a host become commands instead
            // (handled in HostNoun::resolve, not here)
            Resolved::RepoDetail { .. } | Resolved::RepoProviders { .. } | Resolved::RepoWork { .. } => {}
        }
    }
}

/// Two-stage parsing: clap parse produces a partial type, refine produces the full type.
/// Only needed for nouns where clap cannot express the full structure in one pass (e.g. host routing).
pub trait Refinable {
    type Refined;
    fn refine(self) -> Result<Self::Refined, String>;
}
```

- [ ] **Step 4: Create lib.rs**

`crates/flotilla-commands/src/lib.rs`:
```rust
pub mod commands;
pub mod resolved;

pub use resolved::{Refinable, Resolved};
```

Note: `noun` module and `complete` module added in later tasks.

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p flotilla-commands`

- [ ] **Step 6: Commit**

Commit: `feat: create flotilla-commands crate with Resolved and Refinable`

---

### Task 2: Implement repo noun with tests

**Files:**
- Create: `crates/flotilla-commands/src/commands/mod.rs`
- Create: `crates/flotilla-commands/src/commands/repo.rs`

This is the most complex noun — it has subject-before-verb parsing, both commands and queries, and covers most of the current `parse_repo_command` logic (lines 428-487 of `src/main.rs`).

- [ ] **Step 1: Create commands/mod.rs**

`crates/flotilla-commands/src/commands/mod.rs`:
```rust
pub mod repo;
```

- [ ] **Step 2: Write failing tests for repo resolve**

Add to `crates/flotilla-commands/src/commands/repo.rs` a `#[cfg(test)] mod tests` block with tests covering the existing `parse_repo_command` behavior. Key test cases:

```rust
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use flotilla_protocol::{CheckoutTarget, Command, CommandAction, RepoSelector};

    use super::RepoNoun;
    use crate::Resolved;

    fn parse(args: &[&str]) -> RepoNoun {
        RepoNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn repo_add() {
        let resolved = parse(&["repo", "add", "/tmp/test"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::TrackRepoPath { path: PathBuf::from("/tmp/test") },
            })
        );
    }

    #[test]
    fn repo_remove() {
        let resolved = parse(&["repo", "remove", "owner/repo"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::UntrackRepo { repo: RepoSelector::Query("owner/repo".into()) },
            })
        );
    }

    #[test]
    fn repo_refresh_all() {
        let resolved = parse(&["repo", "refresh"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: None },
            })
        );
    }

    #[test]
    fn repo_refresh_specific() {
        let resolved = parse(&["repo", "refresh", "owner/repo"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: Some(RepoSelector::Query("owner/repo".into())) },
            })
        );
    }

    #[test]
    fn repo_query_detail() {
        let resolved = parse(&["repo", "myslug"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::RepoDetail { slug: "myslug".into() });
    }

    #[test]
    fn repo_query_providers() {
        let resolved = parse(&["repo", "myslug", "providers"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::RepoProviders { slug: "myslug".into() });
    }

    #[test]
    fn repo_query_work() {
        let resolved = parse(&["repo", "myslug", "work"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::RepoWork { slug: "myslug".into() });
    }

    #[test]
    fn repo_checkout_existing_branch() {
        let resolved = parse(&["repo", "myslug", "checkout", "feat-x"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("myslug".into()),
                    target: CheckoutTarget::Branch("feat-x".into()),
                    issue_ids: vec![],
                },
            })
        );
    }

    #[test]
    fn repo_checkout_fresh_branch() {
        let resolved = parse(&["repo", "myslug", "checkout", "--fresh", "feat-x"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("myslug".into()),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![],
                },
            })
        );
    }

    #[test]
    fn repo_prepare_terminal() {
        let resolved = parse(&["repo", "myslug", "prepare-terminal", "/tmp/path"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: Some(RepoSelector::Query("myslug".into())),
                action: CommandAction::PrepareTerminalForCheckout {
                    checkout_path: PathBuf::from("/tmp/path"),
                    commands: vec![],
                },
            })
        );
    }

    #[test]
    fn repo_subject_form_refresh() {
        // `repo myslug refresh` — subject used as repo
        let resolved = parse(&["repo", "myslug", "refresh"]).resolve().unwrap();
        assert!(matches!(
            resolved,
            Resolved::Command(Command { action: CommandAction::Refresh { repo: Some(_) }, .. })
        ));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands`
Expected: compilation errors (RepoNoun not defined yet)

- [ ] **Step 4: Implement RepoNoun, RepoVerb, resolve, and Display**

`crates/flotilla-commands/src/commands/repo.rs` — the struct definitions, resolve function, and Display impl. The resolve function mirrors `parse_repo_command` (lines 428-487 of `src/main.rs`), mapping subject + verb to `Resolved` variants.

Key implementation details:
- `subcommand_precedence_over_arg = true` and `subcommand_negates_reqs = true` on `RepoNoun`
- `subject: Option<String>` and `verb: Option<RepoVerb>`
- When both `subject` and `verb` are `None`, return error
- When `subject` is `Some` and `verb` is `None`, return `Resolved::RepoDetail`
- For `RepoVerb::Refresh { repo }`, merge with `subject` — prefer explicit verb arg, fall back to subject
- For `RepoVerb::Checkout`, get repo from `subject` (required — error if missing)
- For `RepoVerb::PrepareTerminal`, set `context_repo` from `subject`
- All noun structs and verb enums derive `Debug, Clone, PartialEq, Eq` (in addition to `Parser`/`Subcommand`) for testing
- `Display` outputs `repo [subject] [verb] [args]`

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 6: Run formatting and clippy**

Run: `cargo +nightly-2026-03-12 fmt -- crates/flotilla-commands/src/**/*.rs`
Run: `cargo clippy -p flotilla-commands --all-targets --locked -- -D warnings`

- [ ] **Step 7: Commit**

Commit: `feat: implement repo noun with resolve and Display`

---

### Task 3: Implement simple nouns (checkout, cr, issue, agent, workspace)

**Files:**
- Create: `crates/flotilla-commands/src/commands/checkout.rs`
- Create: `crates/flotilla-commands/src/commands/cr.rs`
- Create: `crates/flotilla-commands/src/commands/issue.rs`
- Create: `crates/flotilla-commands/src/commands/agent.rs`
- Create: `crates/flotilla-commands/src/commands/workspace.rs`
- Modify: `crates/flotilla-commands/src/commands/mod.rs`

These are simpler nouns — most verbs map directly to a single `CommandAction` variant.

- [ ] **Step 1: Write failing tests for all five nouns**

Each noun gets a test file or `#[cfg(test)] mod tests` with resolve tests. Key cases:

**checkout:**
- `checkout create --branch feat-x` → `Checkout { repo: RepoSelector::Query("".into()), target: Branch("feat-x") }` (empty repo sentinel — dispatch injects from `--repo`/env)
- `checkout create --branch feat-x --fresh` → `Checkout { repo: RepoSelector::Query("".into()), target: FreshBranch("feat-x") }`
- `checkout my-feature remove` → `RemoveCheckout { checkout: CheckoutSelector::Query("my-feature") }`
- `checkout my-feature status` → `FetchCheckoutStatus { branch: "my-feature" }`
- `checkout my-feature status --checkout-path /tmp/wt --cr-id 42` → `FetchCheckoutStatus` with all fields

**cr:**
- `cr 42 open` → `OpenChangeRequest { id: "42" }`
- `cr 42 close` → `CloseChangeRequest { id: "42" }`
- `cr 42 link-issues 1 5 7` → `LinkIssuesToChangeRequest { change_request_id: "42", issue_ids: ["1", "5", "7"] }`
- `pr 42 open` → same as `cr 42 open` (alias)

**issue:**
- `issue 1 open` → `OpenIssue { id: "1" }`
- `issue 1,5,7 suggest-branch` → `GenerateBranchName { issue_keys: ["1", "5", "7"] }`
- `issue search my query` → `SearchIssues { repo: RepoSelector::Query("".into()), query: "my query" }` (repo sentinel — dispatch injects from `--repo`/env). Note: `issue search` maps to `CommandAction::SearchIssues` which is a user-facing command. The deferred "issue viewport" commands are `SetIssueViewport`, `FetchMoreIssues`, `ClearIssueSearch` — UI-internal, not `SearchIssues`.

**agent:**
- `agent claude-1 teleport` → `TeleportSession { session_id: "claude-1", branch: None, checkout_key: None }`
- `agent claude-1 teleport --branch feat` → `TeleportSession { session_id: "claude-1", branch: Some("feat"), checkout_key: None }`
- `agent claude-1 archive` → `ArchiveSession { session_id: "claude-1" }`

**workspace:**
- `workspace feat-ws select` → `SelectWorkspace { ws_ref: "feat-ws" }`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands`
Expected: compilation errors

- [ ] **Step 3: Implement all five nouns**

Each noun follows the pattern from the spec. Implementation details:

- **checkout:** Uses `subcommand_precedence_over_arg` / `subcommand_negates_reqs`. `create` has no subject. `remove`/`status` require subject. For `create`, `checkout create` resolves with `context_repo: None` (dispatch injects it).
- **cr:** `visible_alias = "pr"`. Subject and verb both required. `context_repo: None` (dispatch injects it).
- **issue:** Subject is `Option<String>`. Comma-separated subjects split in resolve. `context_repo: None` for all variants.
- **agent:** Subject and verb required. `context_repo: None`.
- **workspace:** Subject and verb required. `context_repo: None`.

Add all modules to `commands/mod.rs`:
```rust
pub mod agent;
pub mod checkout;
pub mod cr;
pub mod issue;
pub mod repo;
pub mod workspace;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 5: Run formatting and clippy**

Run: `cargo +nightly-2026-03-12 fmt -- crates/flotilla-commands/src/**/*.rs`
Run: `cargo clippy -p flotilla-commands --all-targets --locked -- -D warnings`

- [ ] **Step 6: Commit**

Commit: `feat: implement checkout, cr, issue, agent, workspace nouns`

---

### Task 4: Implement host noun with two-stage parsing

**Files:**
- Create: `crates/flotilla-commands/src/commands/host.rs`
- Create: `crates/flotilla-commands/src/noun.rs`
- Modify: `crates/flotilla-commands/src/commands/mod.rs`
- Modify: `crates/flotilla-commands/src/lib.rs`

Host is the most complex noun — it has its own verbs AND routes other noun commands via two-stage parsing.

- [ ] **Step 1: Create NounCommand enum**

`crates/flotilla-commands/src/noun.rs`:
```rust
use clap::Subcommand;

use crate::commands::{agent::AgentNoun, checkout::CheckoutNoun, cr::CrNoun, issue::IssueNoun, repo::RepoNoun, workspace::WorkspaceNoun};
use crate::Resolved;

/// All domain noun commands. Used by host routing to parse inner commands,
/// and as the top-level dispatch type.
#[derive(Debug, Subcommand)]
pub enum NounCommand {
    Repo(RepoNoun),
    Checkout(CheckoutNoun),
    Cr(CrNoun), // alias "pr" is on CrNoun itself, not here
    Issue(IssueNoun),
    Agent(AgentNoun),
    Workspace(WorkspaceNoun),
    // Host is NOT included — host doesn't nest inside host
}

impl NounCommand {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self {
            NounCommand::Repo(noun) => noun.resolve(),
            NounCommand::Checkout(noun) => noun.resolve(),
            NounCommand::Cr(noun) => noun.resolve(),
            NounCommand::Issue(noun) => noun.resolve(),
            NounCommand::Agent(noun) => noun.resolve(),
            NounCommand::Workspace(noun) => noun.resolve(),
        }
    }
}
```

Update `lib.rs` to add:
```rust
pub mod noun;
pub use noun::NounCommand;
```

- [ ] **Step 2: Write failing tests for host noun**

Tests in `crates/flotilla-commands/src/commands/host.rs`:

```rust
#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction, RepoSelector};

    use super::HostNounPartial;
    use crate::{Refinable, Resolved};

    fn parse(args: &[&str]) -> HostNounPartial {
        HostNounPartial::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn host_list() {
        let resolved = parse(&["host", "list"]).refine().unwrap().resolve().unwrap();
        assert_eq!(resolved, Resolved::HostList);
    }

    #[test]
    fn host_status() {
        let resolved = parse(&["host", "alpha", "status"]).refine().unwrap().resolve().unwrap();
        assert_eq!(resolved, Resolved::HostStatus { host: "alpha".into() });
    }

    #[test]
    fn host_providers() {
        let resolved = parse(&["host", "alpha", "providers"]).refine().unwrap().resolve().unwrap();
        assert_eq!(resolved, Resolved::HostProviders { host: "alpha".into() });
    }

    #[test]
    fn host_refresh_bare() {
        let resolved = parse(&["host", "alpha", "refresh"]).refine().unwrap().resolve().unwrap();
        assert!(matches!(
            resolved,
            Resolved::Command(Command {
                action: CommandAction::Refresh { repo: None },
                ..
            })
        ));
    }

    #[test]
    fn host_refresh_with_repo() {
        let resolved = parse(&["host", "alpha", "refresh", "my-repo"]).refine().unwrap().resolve().unwrap();
        assert!(matches!(
            resolved,
            Resolved::Command(cmd) if cmd.host.is_some()
                && matches!(cmd.action, CommandAction::Refresh { repo: Some(RepoSelector::Query(ref q)) } if q == "my-repo")
        ));
    }

    #[test]
    fn host_routes_repo_command() {
        let resolved = parse(&["host", "feta", "repo", "myslug", "checkout", "main"])
            .refine()
            .unwrap()
            .resolve()
            .unwrap();
        assert!(matches!(
            resolved,
            Resolved::Command(cmd) if cmd.host.as_ref().map(|h| h.as_str()) == Some("feta")
                && matches!(cmd.action, CommandAction::Checkout { .. })
        ));
    }

    #[test]
    fn host_routes_checkout_remove() {
        let resolved = parse(&["host", "alpha", "checkout", "my-feature", "remove"])
            .refine()
            .unwrap()
            .resolve()
            .unwrap();
        assert!(matches!(
            resolved,
            Resolved::Command(cmd) if cmd.host.is_some()
                && matches!(cmd.action, CommandAction::RemoveCheckout { .. })
        ));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 4: Implement HostNounPartial, HostNoun, refine, resolve, Display**

`crates/flotilla-commands/src/commands/host.rs`:

- `HostNounPartial` with `subcommand_precedence_over_arg`, `subcommand_negates_reqs`
- `HostVerbPartial` with List, Status, Providers, Refresh, and `#[command(external_subcommand)] Route(Vec<OsString>)`
- `HostNoun` and `HostVerb` (refined types — not clap derive, plain structs/enums)
- `impl Refinable for HostNounPartial` — maps simple verbs through, parses `Route` tokens via `NounCommand::augment_subcommands` on a temporary clap `Command` then `try_get_matches_from`
- `impl HostNoun { fn resolve(self) -> Result<Resolved, String> }` — delegates to inner NounCommand for Route, calls `resolved.set_host(host)`, handles own verbs directly
- `impl Display` for HostNoun

Add `pub mod host;` to `commands/mod.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 6: Run formatting and clippy**

Run: `cargo +nightly-2026-03-12 fmt -- crates/flotilla-commands/src/**/*.rs`
Run: `cargo clippy -p flotilla-commands --all-targets --locked -- -D warnings`

- [ ] **Step 7: Commit**

Commit: `feat: implement host noun with two-stage parsing`

---

### Task 5: Wire registry CLI into main.rs

**Files:**
- Modify: `src/main.rs` — `Cli` struct, `SubCommand` enum, `main()` dispatch, removal of `normalize_cli_args`/`parse_*` functions/old enums/`run_repo`/`run_host`, test migration

This is the integration task — replace old domain subcommands with noun types.

- [ ] **Step 1: Update Cli struct**

Add `#[arg(long, global = true)] json: bool` to Cli. Remove `json: bool` from Status, Watch, Topology, Refresh variants.

- [ ] **Step 2: Replace domain subcommand variants**

Remove from SubCommand: `Refresh`, `Repo`, `Checkout` (and `CheckoutSubCommand`), `Host`.
Add: `Repo(flotilla_commands::commands::repo::RepoNoun)`, `Checkout(flotilla_commands::commands::checkout::CheckoutNoun)`, `Cr(flotilla_commands::commands::cr::CrNoun)`, `Issue(flotilla_commands::commands::issue::IssueNoun)`, `Agent(flotilla_commands::commands::agent::AgentNoun)`, `Workspace(flotilla_commands::commands::workspace::WorkspaceNoun)`, `Host(flotilla_commands::commands::host::HostNounPartial)`.

Keep infrastructure variants unchanged: Daemon, Status, Watch, Topology, Hook, Hooks.

- [ ] **Step 3: Add `--repo` flag to Cli and implement context injection**

Add to the `Cli` struct:
```rust
/// Repo context for commands that need it (e.g. checkout create, cr close)
#[arg(long)]
repo: Option<String>,
```

Implement a context injection function that fills in missing `repo` fields on `CommandAction` variants that require them. This bridges the gap between the resolve function (which doesn't know the repo) and execution (which requires it):

```rust
fn inject_repo_context(cmd: &mut Command, cli: &Cli) -> Result<()> {
    // If the command already has context_repo or the action has its own repo, skip.
    // Otherwise, resolve from: --repo flag > FLOTILLA_REPO env var > error.
    let repo_selector = match (&cli.repo, std::env::var("FLOTILLA_REPO").ok()) {
        (Some(repo), _) => Some(RepoSelector::Query(repo.clone())),
        (None, Some(repo)) => Some(RepoSelector::Query(repo)),
        (None, None) => None,
    };

    // Patch action variants that need a repo but don't have one
    match &mut cmd.action {
        CommandAction::Checkout { repo, .. } if *repo == RepoSelector::Query(String::new()) => {
            *repo = repo_selector.ok_or_else(|| eyre!("checkout create requires --repo or FLOTILLA_REPO"))?;
        }
        CommandAction::SearchIssues { repo, .. } if cmd.context_repo.is_none() => {
            cmd.context_repo = repo_selector.clone();
            *repo = repo_selector.ok_or_else(|| eyre!("issue search requires --repo or FLOTILLA_REPO"))?;
        }
        // Other variants that need context_repo
        _ => {
            if cmd.context_repo.is_none() {
                cmd.context_repo = repo_selector;
            }
        }
    }
    Ok(())
}
```

Commands that need repo context but don't have it: `checkout create` (sets `CommandAction::Checkout.repo` to empty `RepoSelector::Query("")` in resolve, patched here), `cr open/close/link-issues` (sets `context_repo`), `issue open/suggest-branch/search` (sets `context_repo` and action `repo` field), `agent teleport/archive` (sets `context_repo`), `workspace select` (sets `context_repo`).

- [ ] **Step 4: Add dispatch function**

```rust
async fn dispatch(mut resolved: flotilla_commands::Resolved, cli: &Cli, format: OutputFormat) -> Result<()> {
    use flotilla_commands::Resolved;
    // Inject repo context for commands that need it
    if let Resolved::Command(ref mut cmd) = resolved {
        inject_repo_context(cmd, cli)?;
    }
    match resolved {
        Resolved::Command(cmd) => run_control_command(cli, cmd, format).await,
        Resolved::RepoDetail { slug } => run_repo_detail(cli, &slug, format).await,
        Resolved::RepoProviders { slug } => run_repo_providers(cli, &slug, format).await,
        Resolved::RepoWork { slug } => run_repo_work(cli, &slug, format).await,
        Resolved::HostList => run_host_list(cli, format).await,
        Resolved::HostStatus { host } => run_host_status(cli, &host, format).await,
        Resolved::HostProviders { host } => run_host_providers(cli, &host, format).await,
    }
}
```

Note: `run_repo_detail`, `run_repo_providers`, `run_repo_work`, `run_host_list`, `run_host_status`, `run_host_providers` — these exist today embedded in `run_repo` and `run_host`. Extract them as standalone functions, or inline the logic in `dispatch`.

- [ ] **Step 5: Update main dispatch**

Replace the match in `main()` (lines 172-206):
```rust
let format = OutputFormat::from_json_flag(cli.json);

match cli.command {
    Some(SubCommand::Daemon { .. }) => run_daemon(&cli, ..).await,
    Some(SubCommand::Status) => run_status(&cli, format).await,
    Some(SubCommand::Watch) => run_watch(&cli, format).await,
    Some(SubCommand::Topology) => run_topology_command(&cli, format).await,
    Some(SubCommand::Hook { .. }) => run_hook(&cli, ..).await,
    Some(SubCommand::Hooks { .. }) => run_hooks_command(..).await,

    Some(SubCommand::Repo(noun)) => dispatch(noun.resolve().map_err(|e| eyre!(e))?, &cli, format).await,
    Some(SubCommand::Cr(noun)) => dispatch(noun.resolve().map_err(|e| eyre!(e))?, &cli, format).await,
    Some(SubCommand::Checkout(noun)) => dispatch(noun.resolve().map_err(|e| eyre!(e))?, &cli, format).await,
    Some(SubCommand::Issue(noun)) => dispatch(noun.resolve().map_err(|e| eyre!(e))?, &cli, format).await,
    Some(SubCommand::Agent(noun)) => dispatch(noun.resolve().map_err(|e| eyre!(e))?, &cli, format).await,
    Some(SubCommand::Workspace(noun)) => dispatch(noun.resolve().map_err(|e| eyre!(e))?, &cli, format).await,
    Some(SubCommand::Host(partial)) => {
        use flotilla_commands::Refinable;
        dispatch(partial.refine().and_then(|n| n.resolve()).map_err(|e| eyre!(e))?, &cli, format).await
    }

    None => run_tui(cli).await,
}
```

- [ ] **Step 6: Delete old parsing code**

Remove these functions and types from `src/main.rs`:
- `normalize_cli_args` function
- `find_subcommand_index` function
- `try_parse_cli_from` function — replace with direct `Cli::try_parse()` or `Cli::parse()`
- `parse_repo_command` function
- `parse_host_control_command` function
- `parse_host_command` function
- `RepoCommand`, `RepoQueryCommand`, `HostCommand`, `HostQueryCommand` enums
- `run_repo` and `run_host` functions — replaced by `dispatch`
- `CheckoutSubCommand` enum

Remove unused imports: `OsString`, `CheckoutSelector`, `CheckoutTarget` (now used in flotilla-commands, not main.rs).

**Note:** The `Refresh` top-level subcommand is removed — it becomes `repo refresh`. This is intentional; the project is in a no-backward-compatibility phase. Scripts using `flotilla refresh` must change to `flotilla repo refresh`.

- [ ] **Step 7: Verify it compiles**

Run: `cargo check --workspace --locked`

- [ ] **Step 8: Migrate tests**

Replace the test module (lines 815-954) with equivalent tests using the new noun types. The old tests tested `parse_repo_command` and `parse_host_command` directly and via `try_parse_cli_from` — new tests should use noun `try_parse_from` and `resolve`. Some tests are now redundant with the flotilla-commands crate tests; keep only tests that exercise the main.rs integration (e.g., `Cli::try_parse_from` produces the right SubCommand variant).

Delete tests for `normalize_cli_args` (no longer needed — `--json` is global).

- [ ] **Step 9: Run full test suite**

Run: `cargo test --workspace --locked`

- [ ] **Step 10: Run CI checks**

Run: `cargo +nightly-2026-03-12 fmt --check`
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 11: Commit**

Commit: `feat: wire registry-generated CLI into flotilla binary`

---

### Task 6: Add shell completion engine

**Files:**
- Create: `crates/flotilla-commands/src/complete.rs`
- Modify: `crates/flotilla-commands/src/lib.rs`
- Modify: `src/main.rs` (add Complete and Completions subcommands)

- [ ] **Step 1: Write failing tests for completion engine**

`crates/flotilla-commands/src/complete.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> clap::Command {
        // Build a minimal root command for testing.
        // In the real binary, this is Cli::command().
        use crate::commands::{repo::RepoNoun, checkout::CheckoutNoun, cr::CrNoun, host::HostNounPartial};
        use clap::{Parser, CommandFactory};
        // Build from noun commands + stub infrastructure subcommands
        clap::Command::new("flotilla")
            .subcommand(RepoNoun::command().name("repo"))
            .subcommand(CheckoutNoun::command().name("checkout"))
            .subcommand(CrNoun::command().name("cr"))
            .subcommand(HostNounPartial::command().name("host"))
            .subcommand(clap::Command::new("status"))
            .subcommand(clap::Command::new("daemon"))
    }

    #[test]
    fn empty_input_completes_to_nouns_and_infrastructure() {
        let completions = complete(&test_root(), "", 0);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"repo"));
        assert!(values.contains(&"cr"));
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"host"));
        assert!(values.contains(&"status"));
        assert!(values.contains(&"daemon"));
    }

    #[test]
    fn noun_completes_to_verbs() {
        let completions = complete(&test_root(), "repo ", 5);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"add"));
        assert!(values.contains(&"remove"));
        assert!(values.contains(&"refresh"));
        assert!(values.contains(&"checkout"));
    }

    #[test]
    fn noun_with_subject_completes_to_verbs() {
        let completions = complete(&test_root(), "repo myslug ", 12);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"providers"));
        assert!(values.contains(&"work"));
    }

    #[test]
    fn host_with_subject_completes_to_verbs_and_nouns() {
        let completions = complete(&test_root(), "host feta ", 10);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        // Host verbs
        assert!(values.contains(&"status"));
        assert!(values.contains(&"providers"));
        assert!(values.contains(&"list"));
        // Routable nouns
        assert!(values.contains(&"repo"));
        assert!(values.contains(&"checkout"));
    }

    #[test]
    fn host_routed_noun_completes_to_noun_verbs() {
        let completions = complete(&test_root(), "host feta repo myslug ", 22);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"providers"));
    }

    #[test]
    fn partial_noun_completes() {
        let completions = complete(&test_root(), "ch", 2);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"checkout"));
        assert!(!values.contains(&"repo"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 3: Implement completion engine**

`crates/flotilla-commands/src/complete.rs`:

```rust
use clap::Command;

pub struct CompletionItem {
    pub value: String,
    pub description: Option<String>,
}

/// Complete a command line. `root` is the full clap Command tree,
/// passed in by the binary crate (which owns the full CLI definition).
pub fn complete(root: &Command, line: &str, cursor_pos: usize) -> Vec<CompletionItem> {
    let input = &line[..cursor_pos.min(line.len())];
    let tokens: Vec<&str> = input.split_whitespace().collect();
    // If input ends with space, we're completing the next token
    // If not, we're completing a partial current token
    let trailing_space = input.ends_with(' ') || input.is_empty();
    if trailing_space {
        walk_for_completions(&tokens, &root, 0, "")
    } else {
        let (prefix_tokens, partial) = tokens.split_at(tokens.len().saturating_sub(1));
        let partial = partial.first().copied().unwrap_or("");
        walk_for_completions(prefix_tokens, &root, 0, partial)
    }
}

fn walk_for_completions(tokens: &[&str], cmd: &Command, pos: usize, partial: &str) -> Vec<CompletionItem> {
    if pos >= tokens.len() {
        return filter_completions(valid_next_tokens(cmd), partial);
    }

    let token = tokens[pos];

    // Try matching as subcommand
    if let Some(sub) = cmd.find_subcommand(token) {
        return walk_for_completions(tokens, sub, pos + 1, partial);
    }

    // Host routing: external_subcommands → try noun names
    if cmd.is_allow_external_subcommands_set() {
        if let Some(noun_cmd) = find_noun_command(token) {
            return walk_for_completions(tokens, &noun_cmd, pos + 1, partial);
        }
    }

    // Positional (subject) — consume and continue
    walk_for_completions(tokens, cmd, pos + 1, partial)
}
```

The `complete()` function takes a `&Command` parameter — the binary crate passes `Cli::command()` which includes all subcommands. `find_noun_command` looks up nouns by name and returns their clap `Command` via the noun struct's `CommandFactory::command()` method.

Helper functions: `valid_next_tokens` returns subcommand names + flag names. `filter_completions` filters by prefix.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 5: Add Complete and Completions subcommands to main.rs**

Add to SubCommand enum:
```rust
/// Generate completions (hidden, called by shell scripts)
#[command(hide = true)]
Complete {
    /// The input line to complete
    line: String,
    /// Cursor position within the line
    #[arg(default_value = "0")]
    cursor_pos: usize,
},
/// Output shell completion setup scripts
Completions {
    /// Shell type
    #[arg(value_enum)]
    shell: CompletionShell,
},
```

Define `CompletionShell` as a `clap::ValueEnum`:
```rust
#[derive(Clone, clap::ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}
```

Implement `run_complete` and `run_completions` handlers. `run_complete` builds the root command via `Cli::command()` and calls `flotilla_commands::complete::complete(&root, &line, cursor_pos)`, printing tab-separated output. `run_completions` outputs hardcoded shell scripts (one template per shell) that call `flotilla complete`.

Add dispatch arms in main:
```rust
Some(SubCommand::Complete { line, cursor_pos }) => run_complete(&line, cursor_pos),
Some(SubCommand::Completions { shell }) => run_completions(shell),
```

- [ ] **Step 6: Run full test suite and CI checks**

Run: `cargo test --workspace --locked`
Run: `cargo +nightly-2026-03-12 fmt --check`
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 7: Commit**

Commit: `feat: shell completion engine from command registry`

---

### Task 7: Display round-trip tests and final cleanup

**Files:**
- Modify: `crates/flotilla-commands/src/commands/repo.rs` (add round-trip tests)
- Modify: `crates/flotilla-commands/src/commands/host.rs` (add round-trip tests)
- Modify: various files for cleanup

- [ ] **Step 1: Add Display round-trip tests for each noun**

For each noun, add a test that `parse → display → parse` produces the same result:

```rust
#[test]
fn display_round_trips() {
    let cases = [
        &["repo", "myslug", "checkout", "main"][..],
        &["repo", "add", "/tmp/test"],
        &["repo", "myslug", "providers"],
        &["checkout", "my-feature", "remove"],
        &["cr", "42", "close"],
        &["agent", "claude-1", "teleport"],
        &["workspace", "feat-ws", "select"],
    ];
    for args in &cases {
        // Parse each through its noun, display, re-parse, compare
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-commands`

- [ ] **Step 3: Run full CI suite**

Run: `cargo test --workspace --locked`
Run: `cargo +nightly-2026-03-12 fmt --check`
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 4: Commit**

Commit: `test: Display round-trip tests for registry nouns`
