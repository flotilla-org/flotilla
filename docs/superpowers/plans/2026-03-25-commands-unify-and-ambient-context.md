# Commands Unify and Ambient Context Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify queries and commands into a single dispatch path and add typed ambient context metadata on Resolved (#502, #505, #506).

**Architecture:** Queries become CommandAction variants routed through execute(). Resolved collapses from 10 variants to Ready/NeedsContext with RepoContext and HostResolution enums. CLI dispatch interprets the metadata; TUI changes are deferred to Phase 2.

**Tech Stack:** Rust, clap (noun parsing), flotilla crate ecosystem

**Spec:** `docs/superpowers/specs/2026-03-25-commands-unify-and-ambient-context-design.md`

---

## File Map

| File | Changes |
|------|---------|
| `crates/flotilla-protocol/src/commands.rs` | Add 6 CommandAction query variants, 6 CommandValue result variants, description() arms, update roundtrip tests |
| `crates/flotilla-protocol/src/lib.rs` | Remove 6 Request variants, 6 Response variants |
| `crates/flotilla-commands/src/resolved.rs` | Replace 10-variant Resolved with Ready/NeedsContext + RepoContext + HostResolution |
| `crates/flotilla-commands/src/lib.rs` | Re-export RepoContext and HostResolution |
| `crates/flotilla-commands/src/commands/repo.rs` | resolve() produces Command with Query* actions instead of Resolved query variants |
| `crates/flotilla-commands/src/commands/host.rs` | resolve() produces Command with Query* actions; set_host collapse |
| `crates/flotilla-commands/src/commands/checkout.rs` | resolve() returns NeedsContext with RepoContext::Required |
| `crates/flotilla-commands/src/commands/issue.rs` | resolve() returns NeedsContext with RepoContext::Required/Inferred |
| `crates/flotilla-commands/src/commands/cr.rs` | resolve() returns NeedsContext with RepoContext::Inferred |
| `crates/flotilla-commands/src/commands/agent.rs` | resolve() returns NeedsContext with RepoContext::Inferred |
| `crates/flotilla-commands/src/commands/workspace.rs` | resolve() returns NeedsContext with RepoContext::Inferred |
| `crates/flotilla-core/src/daemon.rs` | Remove 6 query methods from DaemonHandle trait |
| `crates/flotilla-core/src/in_process.rs` | Handle Query* actions as daemon-level commands in execute() |
| `crates/flotilla-core/src/in_process/tests.rs` | Add query execution tests through InProcessDaemon |
| `crates/flotilla-client/src/lib.rs` | Remove 6 query method impls from SocketDaemon |
| `crates/flotilla-client/src/lib/tests.rs` | Update tests that reference removed Request/Response variants |
| `crates/flotilla-daemon/src/server/request_dispatch.rs` | Remove 6 query arms from dispatch() |
| `crates/flotilla-daemon/src/server/tests.rs` | Update tests that reference removed Request/Response variants |
| `src/main.rs` | Simplify dispatch() to Ready/NeedsContext match with RepoContext handling |
| `crates/flotilla-tui/src/cli.rs` | Delete 6 standalone query runners, add CommandValue formatting in run_command and format_command_result |
| `crates/flotilla-tui/src/app/executor.rs` | Add ignore arms for 6 new CommandValue variants |

---

## Task 1: Add Query CommandAction and CommandValue Variants

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs` (enum variants, description(), roundtrip tests)

These are additive changes — nothing breaks.

- [ ] **Step 1: Add CommandAction query variants**

In `crates/flotilla-protocol/src/commands.rs`, add to the `CommandAction` enum after the existing variants:

```rust
// Query commands — read-only operations dispatched through execute()
QueryRepoDetail { repo: RepoSelector },
QueryRepoProviders { repo: RepoSelector },
QueryRepoWork { repo: RepoSelector },
QueryHostList,
QueryHostStatus { host: String },
QueryHostProviders { host: String },
```

- [ ] **Step 2: Add CommandValue result variants**

In the same file, add to the `CommandValue` enum:

```rust
RepoDetail(Box<RepoDetailResponse>),
RepoProviders(Box<RepoProvidersResponse>),
RepoWork(Box<RepoWorkResponse>),
HostList(Box<HostListResponse>),
HostStatus(Box<HostStatusResponse>),
HostProviders(Box<HostProvidersResponse>),
```

Box the responses — they are large structs and CommandValue is passed by value.

- [ ] **Step 3: Add description() arms for query variants**

In the `Command::description()` method, add:

```rust
CommandAction::QueryRepoDetail { .. } => "query repo detail",
CommandAction::QueryRepoProviders { .. } => "query repo providers",
CommandAction::QueryRepoWork { .. } => "query repo work",
CommandAction::QueryHostList => "query host list",
CommandAction::QueryHostStatus { .. } => "query host status",
CommandAction::QueryHostProviders { .. } => "query host providers",
```

- [ ] **Step 4: Update protocol roundtrip tests**

In `crates/flotilla-protocol/src/commands.rs`, update the exhaustive roundtrip tests:
- `command_roundtrip_covers_all_variants` — add entries for all 6 Query\* CommandAction variants
- `command_value_roundtrip_covers_all_variants` — add entries for all 6 query result CommandValue variants
- `command_description_covers_all_variants` — add entries for the 6 new description() arms

These tests use exhaustive matching to ensure every variant is covered, so they will fail to compile without the new arms.

- [ ] **Step 5: Build and verify**

Run: `cargo build --workspace --locked && cargo test -p flotilla-protocol --locked`
Expected: compiles and protocol tests pass

- [ ] **Step 6: Commit**

```
feat: add query CommandAction and CommandValue variants (#502)
```

---

## Task 2: Collapse Resolved Enum

> **Note:** Tasks 2-4 form a single compile unit. The flotilla-commands crate will not compile after Task 2 alone — noun resolve() functions still reference deleted variants. Complete Tasks 3-4 before attempting a build. Commit at the end of Task 4.

**Files:**
- Modify: `crates/flotilla-commands/src/resolved.rs`

- [ ] **Step 1: Replace Resolved enum**

Replace the entire `Resolved` enum and its `set_host()` method with:

```rust
use flotilla_protocol::{Command, HostName};

/// Output of noun resolution — what the dispatch layer acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A command to send to the daemon for execution.
    Command(Command),
    /// A command that requires repo context injection before dispatch.
    /// The action contains SENTINEL `RepoSelector::Query("")` fields.
    RequiresRepoContext(Command),
}

impl Resolved {
    /// Set the target host on a resolved command.
    pub fn set_host(&mut self, host: String) {
        match self {
            Resolved::Command(cmd) | Resolved::RequiresRepoContext(cmd) => {
                cmd.host = Some(HostName::new(&host));
            }
        }
    }
}
```

- [ ] **Step 2: Verify flotilla-commands crate compiles**

Run: `cargo build -p flotilla-commands --locked`
Expected: FAIL — noun resolve() functions still reference deleted variants (RepoDetail, HostList, etc.). This is expected; we fix them in Tasks 3-4.

---

## Task 3: Update RepoNoun Resolve and Tests

**Files:**
- Modify: `crates/flotilla-commands/src/commands/repo.rs`

- [ ] **Step 1: Update resolve tests first**

In `repo.rs` tests, update the three query tests to expect Command with Query* actions:

```rust
#[test]
fn repo_query_detail() {
    let resolved = parse(&["repo", "myslug"]).resolve().unwrap();
    assert_eq!(
        resolved,
        Resolved::Command(Command {
            host: None,
            context_repo: None,
            action: CommandAction::QueryRepoDetail { repo: RepoSelector::Query("myslug".into()) },
        })
    );
}

#[test]
fn repo_query_providers() {
    let resolved = parse(&["repo", "myslug", "providers"]).resolve().unwrap();
    assert_eq!(
        resolved,
        Resolved::Command(Command {
            host: None,
            context_repo: None,
            action: CommandAction::QueryRepoProviders { repo: RepoSelector::Query("myslug".into()) },
        })
    );
}

#[test]
fn repo_query_work() {
    let resolved = parse(&["repo", "myslug", "work"]).resolve().unwrap();
    assert_eq!(
        resolved,
        Resolved::Command(Command {
            host: None,
            context_repo: None,
            action: CommandAction::QueryRepoWork { repo: RepoSelector::Query("myslug".into()) },
        })
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands --locked -- repo_query`
Expected: FAIL — resolve() still produces old variants

- [ ] **Step 3: Update RepoNoun::resolve()**

Change the three query arms in `resolve()`:

For `Providers` (currently `Resolved::RepoProviders { slug }`):
```rust
(Some(subject), Some(RepoVerb::Providers)) => Ok(Resolved::Command(Command {
    host: None,
    context_repo: None,
    action: CommandAction::QueryRepoProviders { repo: RepoSelector::Query(subject) },
})),
```

For `Work` (currently `Resolved::RepoWork { slug }`):
```rust
(Some(subject), Some(RepoVerb::Work)) => Ok(Resolved::Command(Command {
    host: None,
    context_repo: None,
    action: CommandAction::QueryRepoWork { repo: RepoSelector::Query(subject) },
})),
```

For subject-only / detail (currently `Resolved::RepoDetail { slug }`):
```rust
(Some(subject), None) => Ok(Resolved::Command(Command {
    host: None,
    context_repo: None,
    action: CommandAction::QueryRepoDetail { repo: RepoSelector::Query(subject) },
})),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands --locked -- repo_query`
Expected: PASS

- [ ] **Step 5: Run all repo tests**

Run: `cargo test -p flotilla-commands --locked -- repo`
Expected: All pass (round-trip tests should be unaffected — they test Display, not Resolved)

- [ ] **Step 6: Commit**

```
refactor: repo queries produce Command with Query* actions (#502)
```

---

## Task 4: Update HostNoun Resolve and Tests

**Files:**
- Modify: `crates/flotilla-commands/src/commands/host.rs`

- [ ] **Step 1: Update host resolve tests**

Update the query tests to expect Command:

```rust
#[test]
fn host_list() {
    let resolved = parse_and_resolve(&["host", "list"]);
    assert_eq!(
        resolved,
        Resolved::Command(Command {
            host: None,
            context_repo: None,
            action: CommandAction::QueryHostList,
        })
    );
}

#[test]
fn host_status() {
    let resolved = parse_and_resolve(&["host", "alpha", "status"]);
    assert_eq!(
        resolved,
        Resolved::Command(Command {
            host: None,
            context_repo: None,
            action: CommandAction::QueryHostStatus { host: "alpha".into() },
        })
    );
}

#[test]
fn host_providers() {
    let resolved = parse_and_resolve(&["host", "alpha", "providers"]);
    assert_eq!(
        resolved,
        Resolved::Command(Command {
            host: None,
            context_repo: None,
            action: CommandAction::QueryHostProviders { host: "alpha".into() },
        })
    );
}
```

Update the three host-routed repo query tests. After #502, `host feta repo myslug providers` becomes `Command { host: Some("feta"), action: QueryRepoProviders { repo } }` — no more HostRepoProviders variant:

```rust
#[test]
fn host_routed_repo_query_becomes_host_targeted() {
    let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "providers"]);
    assert!(matches!(
        resolved,
        Resolved::Command(cmd) if cmd.host.as_ref().map(|h| h.as_str()) == Some("feta")
            && matches!(cmd.action, CommandAction::QueryRepoProviders { ref repo } if *repo == RepoSelector::Query("myslug".into()))
    ));
}

#[test]
fn host_routed_repo_detail_becomes_host_targeted() {
    let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug"]);
    assert!(matches!(
        resolved,
        Resolved::Command(cmd) if cmd.host.as_ref().map(|h| h.as_str()) == Some("feta")
            && matches!(cmd.action, CommandAction::QueryRepoDetail { ref repo } if *repo == RepoSelector::Query("myslug".into()))
    ));
}

#[test]
fn host_routed_repo_work_becomes_host_targeted() {
    let resolved = parse_and_resolve(&["host", "feta", "repo", "myslug", "work"]);
    assert!(matches!(
        resolved,
        Resolved::Command(cmd) if cmd.host.as_ref().map(|h| h.as_str()) == Some("feta")
            && matches!(cmd.action, CommandAction::QueryRepoWork { ref repo } if *repo == RepoSelector::Query("myslug".into()))
    ));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands --locked -- host`
Expected: FAIL — resolve() still produces old variants

- [ ] **Step 3: Update HostNoun::resolve()**

Change the three query arms:

```rust
HostVerb::List => Ok(Resolved::Command(Command {
    host: None,
    context_repo: None,
    action: CommandAction::QueryHostList,
})),
HostVerb::Status => {
    let host = subject.ok_or("host status requires a host name")?;
    Ok(Resolved::Command(Command {
        host: None,
        context_repo: None,
        action: CommandAction::QueryHostStatus { host },
    }))
}
HostVerb::Providers => {
    let host = subject.ok_or("host providers requires a host name")?;
    Ok(Resolved::Command(Command {
        host: None,
        context_repo: None,
        action: CommandAction::QueryHostProviders { host },
    }))
}
```

The `Route` arm and `set_host()` call stay the same — `set_host()` now just sets `Command.host` on both variants.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands --locked -- host`
Expected: All pass

- [ ] **Step 5: Update CheckoutNoun for RequiresRepoContext**

In `crates/flotilla-commands/src/commands/checkout.rs`, change the `Create` resolve arm to return `RequiresRepoContext` instead of `Command`:

```rust
// Was: Ok(Resolved::Command(Command { ... }))
Ok(Resolved::RequiresRepoContext(Command {
    host: None,
    context_repo: None,
    action: CommandAction::Checkout { repo: RepoSelector::Query("".into()), target, issue_ids: vec![] },
}))
```

Update the checkout create tests (`checkout_create_branch`, `checkout_create_fresh_branch`) to expect `RequiresRepoContext(...)`.

- [ ] **Step 6: Update IssueNoun for RequiresRepoContext**

In `crates/flotilla-commands/src/commands/issue.rs`, change the `Search` resolve arm:

```rust
// Was: Ok(Resolved::Command(Command { ... }))
Ok(Resolved::RequiresRepoContext(Command {
    host: None,
    context_repo: None,
    action: CommandAction::SearchIssues { repo: RepoSelector::Query("".into()), query },
}))
```

Update the `issue_search` test to expect `RequiresRepoContext(...)`.

- [ ] **Step 7: Build and run all flotilla-commands tests**

Run: `cargo test -p flotilla-commands --locked`
Expected: All pass. No remaining references to deleted Resolved variants.

- [ ] **Step 8: Commit**

```
refactor: host/checkout/issue nouns use collapsed Resolved (#502)
```

---

## Task 5: Update CLI Dispatch and Output

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Simplify dispatch() in main.rs**

Replace the entire `dispatch()` function body with:

```rust
async fn dispatch(resolved: flotilla_commands::Resolved, cli: &Cli, format: OutputFormat) -> Result<()> {
    use flotilla_commands::Resolved;
    reset_sigpipe();
    let mut cmd = match resolved {
        Resolved::Command(cmd) => cmd,
        Resolved::RequiresRepoContext(cmd) => cmd,
    };
    inject_repo_context(&mut cmd, cli)?;
    run_control_command(cli, cmd, format).await
}
```

- [ ] **Step 2: Add query result formatting in cli.rs run_command**

In `crates/flotilla-tui/src/cli.rs`, in the `run_command` function's `CommandFinished` handler, add formatting for the new result variants. Find the existing match on `result` inside the `CommandFinished` arm and add the query cases before the final return:

```rust
Ok(ref event @ DaemonEvent::CommandFinished { command_id: id, ref result, .. }) if id == command_id => {
    // Existing human format for non-query events
    match result {
        CommandValue::RepoDetail(detail) => match format {
            OutputFormat::Human => print!("{}", format_repo_detail_human(detail)),
            OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(detail)),
        },
        CommandValue::RepoProviders(providers) => match format {
            OutputFormat::Human => print!("{}", format_repo_providers_human(providers)),
            OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(providers)),
        },
        CommandValue::RepoWork(work) => match format {
            OutputFormat::Human => print!("{}", format_repo_work_human(work)),
            OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(work)),
        },
        CommandValue::HostList(hosts) => match format {
            OutputFormat::Human => print!("{}", format_host_list_human(hosts)),
            OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(hosts)),
        },
        CommandValue::HostStatus(status) => match format {
            OutputFormat::Human => print!("{}", format_host_status_human(status)),
            OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(status)),
        },
        CommandValue::HostProviders(providers) => match format {
            OutputFormat::Human => print!("{}", format_host_providers_human(providers)),
            OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(providers)),
        },
        _ => {
            // existing event formatting
            match format {
                OutputFormat::Human => println!("{}", format_event_human(event)),
                OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(&result)),
            }
        }
    }
    // ... existing return logic
}
```

The `format_repo_detail_human`, `format_host_list_human` etc. functions already exist in cli.rs — they are used by the standalone runners. Keep them, just stop exporting the standalone `run_*` functions.

- [ ] **Step 3: Update format_command_result**

In `crates/flotilla-tui/src/cli.rs`, the `format_command_result` function (around line 199) exhaustively matches `CommandValue`. Add arms for the 6 new variants:

```rust
CommandValue::RepoDetail(ref detail) => format_repo_detail_human(detail),
CommandValue::RepoProviders(ref providers) => format_repo_providers_human(providers),
CommandValue::RepoWork(ref work) => format_repo_work_human(work),
CommandValue::HostList(ref hosts) => format_host_list_human(hosts),
CommandValue::HostStatus(ref status) => format_host_status_human(status),
CommandValue::HostProviders(ref providers) => format_host_providers_human(providers),
```

- [ ] **Step 4: Delete standalone query runner functions**

Delete from `crates/flotilla-tui/src/cli.rs`:
- `run_repo_detail()`
- `run_repo_providers()`
- `run_repo_work()`
- `run_host_list()`
- `run_host_status()`
- `run_host_providers()`

Keep the `format_*_human` helper functions — they're now called from `run_command` and `format_command_result`.

- [ ] **Step 5: Build and verify**

Run: `cargo build --workspace --locked`
Expected: Will still have the old DaemonHandle query methods on the trait (unused by CLI now but still present). Remaining callers to verify: `request_dispatch.rs` (uses trait methods for Request dispatch — addressed in Task 7), `SocketDaemon` (implements trait — addressed in Task 6). The build should succeed at this point since the trait methods still exist.

- [ ] **Step 6: Commit**

```
refactor: simplify CLI dispatch to two-arm Resolved match (#502)
```

---

## Task 6: Remove DaemonHandle Query Methods and Add Query Execution

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-client/src/lib.rs`

- [ ] **Step 1: Remove query methods from DaemonHandle trait**

In `crates/flotilla-core/src/daemon.rs`, remove these methods from the trait:
- `get_repo_detail()`
- `get_repo_providers()`
- `get_repo_work()`
- `list_hosts()`
- `get_host_status()`
- `get_host_providers()`

Keep: `get_state`, `list_repos`, `get_status`, `get_topology`, `subscribe`, `replay_since`, `execute`, `cancel`.

- [ ] **Step 2: Make removed methods private on InProcessDaemon**

In `crates/flotilla-core/src/in_process.rs`, the implementations of the removed methods should become private helpers (rename or move to a private `impl InProcessDaemon` block). Keep their logic — it's called by execute() in the next step.

- [ ] **Step 3: Add Query\* handling in InProcessDaemon::execute()**

In the `execute()` method, add a new match block after the existing inline issue commands (after the `ClearIssueSearch` arm, before the `TrackRepoPath` block). Follow the same pattern as `TrackRepoPath` — emit CommandStarted, call the internal method, emit CommandFinished:

```rust
CommandAction::QueryRepoDetail { ref repo } => {
    let result = match self.repo_detail_internal(repo).await {
        Ok(detail) => CommandValue::RepoDetail(Box::new(detail)),
        Err(message) => CommandValue::Error { message },
    };
    // emit CommandStarted + CommandFinished with result
    // (follow the TrackRepoPath pattern)
    return Ok(id);
}
// Same pattern for QueryRepoProviders, QueryRepoWork, QueryHostList, QueryHostStatus, QueryHostProviders
```

For host queries (`QueryHostList`, `QueryHostStatus`, `QueryHostProviders`), the internal methods don't need a repo path — they read from the host registry directly.

- [ ] **Step 4: Remove query methods from SocketDaemon**

In `crates/flotilla-client/src/lib.rs`, remove the implementations of the 6 query methods. They are no longer on the trait.

- [ ] **Step 5: Build and verify**

Run: `cargo build --workspace --locked`
Expected: May fail if server/request_dispatch still references the removed trait methods or Request variants. Fix in Task 7.

- [ ] **Step 6: Commit**

```
refactor: remove DaemonHandle query methods, handle queries in execute() (#502)
```

---

## Task 7: Remove Protocol Query Request/Response Variants and Server Arms

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-daemon/src/server/request_dispatch.rs`
- Modify: `crates/flotilla-daemon/src/server/tests.rs`
- Modify: `crates/flotilla-client/src/lib.rs` (test module)
- Modify: `crates/flotilla-tui/src/app/executor.rs`

- [ ] **Step 1: Remove Request/Response query variants**

In `crates/flotilla-protocol/src/lib.rs`:

Remove from `Request`:
- `GetRepoDetail { slug: String }`
- `GetRepoProviders { slug: String }`
- `GetRepoWork { slug: String }`
- `ListHosts`
- `GetHostStatus { host: String }`
- `GetHostProviders { host: String }`

Remove from `Response`:
- `GetRepoDetail(RepoDetailResponse)`
- `GetRepoProviders(RepoProvidersResponse)`
- `GetRepoWork(RepoWorkResponse)`
- `ListHosts(HostListResponse)`
- `GetHostStatus(HostStatusResponse)`
- `GetHostProviders(HostProvidersResponse)`

- [ ] **Step 2: Remove RequestDispatcher query arms**

In `crates/flotilla-daemon/src/server/request_dispatch.rs`, remove the match arms for the deleted Request variants. Query traffic now arrives as `Request::Execute { command }` and routes through RemoteCommandRouter.

- [ ] **Step 3: Update daemon server tests**

In `crates/flotilla-daemon/src/server/tests.rs`, find all tests that reference the removed Request variants (`GetRepoDetail`, `GetRepoProviders`, `GetRepoWork`, `ListHosts`, `GetHostStatus`, `GetHostProviders`). Update them to send queries as `Request::Execute { command: Command { action: CommandAction::QueryRepoDetail { .. }, .. } }` instead.

- [ ] **Step 4: Update client tests**

In `crates/flotilla-client/src/lib/tests.rs` (or `crates/flotilla-client/src/lib.rs` test module), find tests that reference `Request::ListHosts`, `Response::ListHosts`, and other removed variants. Update to use the new query Command path.

- [ ] **Step 5: Add ignore arms in TUI executor.rs**

In `crates/flotilla-tui/src/app/executor.rs`, in the `handle_result` function's match on `CommandValue`, add:

```rust
CommandValue::RepoDetail(_)
| CommandValue::RepoProviders(_)
| CommandValue::RepoWork(_)
| CommandValue::HostList(_)
| CommandValue::HostStatus(_)
| CommandValue::HostProviders(_) => {
    tracing::debug!("query result reached TUI handler — ignoring");
}
```

- [ ] **Step 6: Build and run full test suite**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: All pass. This completes #502.

- [ ] **Step 7: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 8: Run fmt check**

Run: `cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 9: Commit**

```
refactor: remove protocol query Request/Response variants, complete #502
```

---

## Task 8: Add Query Execution Tests Through InProcessDaemon

**Files:**
- Modify: `crates/flotilla-core/src/in_process/tests.rs` (or create a new test that uses InProcessDaemon)

The spec requires testing that Query\* CommandActions through `execute()` produce correct CommandValue results.

- [ ] **Step 1: Write a test for QueryRepoDetail through execute()**

Use the existing `InProcessDaemon` test infrastructure. Create an InProcessDaemon, track a repo, then execute a `QueryRepoDetail` command and verify the result is `CommandValue::RepoDetail(...)`.

The test pattern:
1. Create InProcessDaemon with a test repo tracked
2. Call `daemon.execute(Command { action: QueryRepoDetail { repo: RepoSelector::Query("slug") }, .. })`
3. Subscribe and receive the CommandFinished event
4. Assert the result matches `CommandValue::RepoDetail(_)`

- [ ] **Step 2: Write a test for QueryHostList**

Similar pattern but for host queries — no repo needed:
1. Create InProcessDaemon
2. Execute `QueryHostList`
3. Assert result matches `CommandValue::HostList(_)`

- [ ] **Step 3: Run the tests**

Run: `cargo test -p flotilla-core --locked -- query`
Expected: PASS

- [ ] **Step 4: Commit**

```
test: add query execution tests through InProcessDaemon (#502)
```

---

## Task 9: Add RepoContext and HostResolution Enums

**Files:**
- Modify: `crates/flotilla-commands/src/resolved.rs`
- Modify: `crates/flotilla-commands/src/lib.rs`

This begins #506 work.

- [ ] **Step 1: Add the enums above the Resolved definition**

```rust
/// How a command's repo context should be filled by the dispatch environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoContext {
    /// Action has SENTINEL `RepoSelector::Query("")` fields that must be filled.
    /// Also sets `context_repo`. Errors if no repo is available.
    Required,
    /// Command needs `context_repo` on the Command envelope for daemon routing.
    /// Set from dispatch environment if available; no error if unavailable.
    Inferred,
}

/// How a command's target host should be resolved by the dispatch environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostResolution {
    /// No host needed — runs locally.
    Local,
    /// The user's chosen provisioning target (TUI: ui.target_host; CLI: host routing).
    ProvisioningTarget,
    /// The host where the subject item lives.
    SubjectHost,
    /// The host where the provider runs (remote-only repos route to provider host).
    ProviderHost,
}
```

- [ ] **Step 2: Add re-exports in lib.rs**

In `crates/flotilla-commands/src/lib.rs`, add to the existing `pub use resolved::` line:

```rust
pub use resolved::{HostResolution, Refinable, RepoContext, Resolved};
```

- [ ] **Step 3: Build to verify**

Run: `cargo build -p flotilla-commands --locked`
Expected: Compiles (additive, nothing uses these yet)

- [ ] **Step 4: Commit**

```
feat: add RepoContext and HostResolution enums (#506)
```

---

## Task 10: Reshape Resolved to Ready/NeedsContext

**Files:**
- Modify: `crates/flotilla-commands/src/resolved.rs`
- Modify: `crates/flotilla-commands/src/commands/repo.rs`
- Modify: `crates/flotilla-commands/src/commands/host.rs`
- Modify: `crates/flotilla-commands/src/commands/checkout.rs`
- Modify: `crates/flotilla-commands/src/commands/issue.rs`
- Modify: `crates/flotilla-commands/src/commands/cr.rs`
- Modify: `crates/flotilla-commands/src/commands/agent.rs`
- Modify: `crates/flotilla-commands/src/commands/workspace.rs`
- Modify: `src/main.rs`

This is the core #506 change. All noun resolve() functions and CLI dispatch update together.

- [ ] **Step 1: Update resolve tests for all nouns**

Update tests across all noun files per the context table in the spec. Key changes:

**checkout.rs tests:** `RequiresRepoContext(cmd)` → `NeedsContext { command: cmd, repo: RepoContext::Required, host: HostResolution::ProvisioningTarget }`

**issue.rs tests:**
- `issue_search`: `RequiresRepoContext(cmd)` → `NeedsContext { repo: RepoContext::Required, host: HostResolution::Local, .. }`
- `issue_open`: `Command(cmd)` → `NeedsContext { repo: RepoContext::Inferred, host: HostResolution::ProviderHost, .. }`
- `issue_suggest_branch_multiple`: `Command(cmd)` → `NeedsContext { repo: RepoContext::Inferred, host: HostResolution::ProvisioningTarget, .. }`

**cr.rs tests:** `Command(cmd)` → `NeedsContext { repo: RepoContext::Inferred, host: HostResolution::ProviderHost, .. }` for open, close, link-issues

**agent.rs tests:**
- `agent_archive`: → `NeedsContext { repo: RepoContext::Inferred, host: HostResolution::ProviderHost, .. }`
- `agent_teleport*`: → `NeedsContext { repo: RepoContext::Inferred, host: HostResolution::Local, .. }`

**workspace.rs tests:** `Command(cmd)` → `NeedsContext { repo: RepoContext::Inferred, host: HostResolution::Local, .. }`

**repo.rs tests:**
- Query tests (`repo_query_detail`, `repo_query_providers`, `repo_query_work`): stay `Ready(cmd)` — explicit repo selector
- `repo_add`, `repo_remove`, `repo_refresh_*`: stay `Ready(cmd)` — daemon-level
- `repo_checkout_*`: → `Ready(cmd)` — explicit repo in action (not SENTINEL)
- `repo_prepare_terminal`: needs assessment — check if it uses SENTINEL

**host.rs tests:** Query tests stay as Command → Ready. Routed command tests already use `matches!` on `Resolved::Command` → change to `Resolved::Ready` or `Resolved::NeedsContext` as appropriate.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands --locked`
Expected: FAIL — Resolved still has old shape

- [ ] **Step 3: Replace Resolved enum**

In `resolved.rs`, replace the enum:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Fully resolved — dispatch directly.
    Ready(Command),
    /// Needs ambient context from the dispatch environment.
    NeedsContext {
        command: Command,
        repo: RepoContext,
        host: HostResolution,
    },
}

impl Resolved {
    pub fn set_host(&mut self, host: String) {
        match self {
            Resolved::Ready(cmd) => cmd.host = Some(HostName::new(&host)),
            Resolved::NeedsContext { command, .. } => command.host = Some(HostName::new(&host)),
        }
    }
}
```

- [ ] **Step 4: Update all noun resolve() functions**

Apply the context table. Key pattern per noun:

**repo.rs:** Queries and daemon-level commands → `Ready`. Checkout with explicit repo → `Ready`. PrepareTerminal → `NeedsContext { repo: Inferred, host: SubjectHost }` (current code sets context_repo explicitly via `RepoSelector::Query(subject)`, so this is actually `Ready` — but the spec says SubjectHost, and the TUI intent uses `item_host_repo_command`. Set to `Ready` since the repo is explicit in the noun form `repo <slug> prepare-terminal <path>`).

**checkout.rs:** Create → `NeedsContext { repo: Required, host: ProvisioningTarget }`. Remove/Status → `NeedsContext { repo: Inferred, host: SubjectHost }`.

**cr.rs:** All verbs → `NeedsContext { repo: Inferred, host: ProviderHost }`.

**issue.rs:** Open → `NeedsContext { repo: Inferred, host: ProviderHost }`. SuggestBranch → `NeedsContext { repo: Inferred, host: ProvisioningTarget }`. Search → `NeedsContext { repo: Required, host: Local }`.

**agent.rs:** Teleport → `NeedsContext { repo: Inferred, host: Local }`. Archive → `NeedsContext { repo: Inferred, host: ProviderHost }`.

**workspace.rs:** Select → `NeedsContext { repo: Inferred, host: Local }`.

**host.rs:** Queries → `Ready`. Refresh → `Ready` (already has host set). Route → resolve inner, call `set_host()`.

For host-routed commands, the inner noun may resolve to `NeedsContext`. `set_host()` sets `Command.host` on the NeedsContext's command field. The `HostResolution` metadata from the inner noun is preserved — `set_host()` does not override it. This is correct: the CLI dispatch ignores `HostResolution` (uses `..`), and the TUI's Phase 2 `tui_dispatch` should check if `Command.host` is already set before calling `resolve_host()`.

- [ ] **Step 5: Update main.rs dispatch**

```rust
async fn dispatch(resolved: flotilla_commands::Resolved, cli: &Cli, format: OutputFormat) -> Result<()> {
    use flotilla_commands::Resolved;
    use flotilla_commands::RepoContext;
    reset_sigpipe();
    match resolved {
        Resolved::Ready(cmd) => run_control_command(cli, cmd, format).await,
        Resolved::NeedsContext { mut command, repo, .. } => {
            match repo {
                RepoContext::Required => inject_repo_context(&mut command, cli)?,
                RepoContext::Inferred => set_context_repo(&mut command, cli),
            }
            run_control_command(cli, command, format).await
        }
    }
}
```

Add the `set_context_repo` helper (extracts the non-error fallthrough from current `inject_repo_context`):

```rust
fn set_context_repo(cmd: &mut Command, cli: &Cli) {
    if cmd.context_repo.is_some() {
        return;
    }
    let repo_selector = match (&cli.repo, std::env::var("FLOTILLA_REPO").ok()) {
        (Some(repo), _) => Some(RepoSelector::Query(repo.clone())),
        (None, Some(repo)) if !repo.is_empty() => Some(RepoSelector::Query(repo)),
        _ => None,
    };
    cmd.context_repo = repo_selector;
}
```

Simplify `inject_repo_context` to only handle SENTINEL cases (it can still call `set_context_repo` internally).

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands --locked`
Expected: All pass

- [ ] **Step 7: Build full workspace**

Run: `cargo build --workspace --locked`
Expected: Compiles

- [ ] **Step 8: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: All pass

- [ ] **Step 9: Run CI gates**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 10: Commit**

```
feat: reshape Resolved to Ready/NeedsContext with RepoContext and HostResolution (#506)
```

---

## Task 11: Final Verification

- [ ] **Step 1: Run the full CI gate**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: All three pass cleanly.

- [ ] **Step 2: Verify host-routed query works end-to-end**

Test that `host feta repo myslug providers` resolves correctly:

```bash
cargo test -p flotilla-commands --locked -- host_routed_repo_query
```

Verify the resolve chain: HostNounPartial → refine → HostNoun → resolve inner → set_host → Ready(Command { host: Some("feta"), action: QueryRepoProviders { ... } }).

- [ ] **Step 3: Spot-check snapshot tests**

Run: `cargo test --workspace --locked 2>&1 | grep -i snapshot`

If any snapshot tests fail, investigate the change. Accept only if the change is an intended consequence of the Resolved reshaping.

- [ ] **Step 4: Commit any final fixups**

If needed, commit with appropriate message.
