# Shared Command Registry Phase 2 — Design Spec

**Issue:** #477, #401
**Date:** 2026-03-25
**Parent spec:** `docs/superpowers/specs/2026-03-24-shared-command-registry-design.md`
**Phase 1 spec:** `docs/superpowers/specs/2026-03-24-shared-command-registry-phase1-design.md`
**Prerequisites:** #502 (unify queries and commands), #506 (ambient context on command definitions)
**Prerequisite spec:** `docs/superpowers/specs/2026-03-25-commands-unify-and-ambient-context-design.md`

## Goal

Wire the shared command registry into the TUI. The command palette becomes a noun-verb parser with position-aware completions. Key-binding-triggered actions echo their resolved command in the status bar. Intents become thin adapters that construct command strings and resolve through the registry. Existing palette entries gain argument support.

## Prerequisites (landed)

### #502: Unify queries and commands (PR #510, merged)

Queries are `CommandAction` variants routed through the same `Command { host }` path as mutations. The 9 query variants on `Resolved` are gone.

### #506: Ambient context on command definitions (PR #510, merged)

Each command declares what ambient context it needs — repo (`RepoContext::Required` / `Inferred`), host (`HostResolution`) — via typed metadata on `Resolved::NeedsContext`. #505 subsumed.

### #464: Step-level remote routing (PR #513, merged)

`Command.host` is now consumed by `build_plan()` which stamps steps with `StepHost::Remote(host)`. Remote steps execute via `RemoteStepExecutor` trait. This means the `HostResolution` → `Command.host` pipeline feeds directly into step routing.

### Landed types:

```rust
pub enum RepoContext {
    Required,  // SENTINEL RepoSelector::Query("") must be filled; error if unavailable
    Inferred,  // Set context_repo if available; no error if unavailable
}

pub enum HostResolution {
    Local,
    ProvisioningTarget,
    SubjectHost,
    ProviderHost,
}

pub enum Resolved {
    Ready(Command),
    NeedsContext {
        command: Command,
        repo: RepoContext,
        host: HostResolution,
    },
}
```

This replaces the current TUI-specific routing helpers (`targeted_command`, `item_host_repo_command`, `provider_repo_command`) with a generic mechanism. #505 (`RequiresRepoContext`) is subsumed by this.

All prerequisites are merged. The palette has a single dispatch path with correct host and repo resolution.

## Palette Overhaul

### Two entry points

| Key | Mode | Behavior |
|-----|------|----------|
| `/` | Global | Opens palette empty. All nouns and global commands available. |
| `?` | Contextual | Opens palette pre-filled with noun + subject from the selected work item (e.g., `cr #42 `). Cursor positioned after the subject, verb completions shown. |

`?` currently triggers `ToggleHelp`. Help moves to `h` (freeing `h` from `CycleHost`, which becomes the palette-local `host <name>` command). `h` is a natural mnemonic for help, and `CycleHost` is being replaced by argument-bearing palette commands anyway. The `h` key is also being freed in anticipation of the host noun becoming "provisioning target" when sandbox/VM/container provisioning lands.

### Pre-fill mapping

When `?` is pressed, the selected `WorkItem` maps to a noun + subject:

| Work item state | Pre-fill |
|-----------------|----------|
| Has checkout key | `checkout <branch> ` |
| Has CR key | `cr <id> ` |
| Has issue keys | `issue <id> ` |
| Has agent/session | `agent <id> ` |
| Has workspace ref | `workspace <ref> ` |
| Multiple signals | Pick the most specific — prefer CR > checkout > issue > agent > workspace |

If no work item is selected (selection cleared), `?` behaves like `/` (opens empty).

### Clearing selection

Today the table always has a selected row. To open an empty palette via `?`, or to indicate "no context," the user needs to clear the selection. `Esc` in `Normal` mode clears the table selection. Pressing `j`/`k` restores it.

### Parse flow

The palette parses user text through the registry. `NounCommand` derives `Subcommand` (not `Parser`), so it does not implement `try_parse_from` directly. A convenience function in `flotilla-commands` encapsulates the clap ceremony:

```rust
/// Parse a token sequence as a noun-verb command.
pub fn parse_noun_command(tokens: &[&str]) -> Result<NounCommand, String> {
    let cmd = <NounCommand as Subcommand>::augment_subcommands(
        Command::new("flotilla").no_binary_name(true)
    );
    let matches = cmd.try_get_matches_from(tokens).map_err(|e| e.to_string())?;
    <NounCommand as FromArgMatches>::from_arg_matches(&matches).map_err(|e| e.to_string())
}
```

Note: `no_binary_name(true)` is required so clap treats the first token as a subcommand, not as argv[0]. The Phase 1 host router already uses this pattern (see `host.rs` refine logic).

The palette parse flow:

```
user text ("cr #42 close")
    ↓ tokenize with shlex (shell-style quoting: "path with spaces" works)
    ↓ try palette-local commands first (layout, theme, search, target, etc.)
    ↓ if first token is "host": parse_host_command(tokens)
    ↓ otherwise: parse_noun_command(tokens)
    ↓ resolve()
Resolved::Ready(cmd) or Resolved::NeedsContext { command, repo, host }
    ↓ tui_dispatch: fill repo from active tab, resolve host via HostResolution
    ↓ dispatch to daemon
```

**Precedence rule:** Palette-local commands are tried first. Their names (`layout`, `theme`, `target`, `search`, `refresh`, etc.) don't conflict with noun names. If no palette-local command matches the first token, the input is parsed as a noun-verb command.

**Host parsing:** `NounCommand` excludes `Host` because host uses two-stage parsing (`HostNounPartial` → `refine()` → `HostNoun`). The palette needs a separate entry point for host commands:

```rust
/// Parse a token sequence starting with "host".
/// The first element ("host") is consumed by clap as argv[0].
pub fn parse_host_command(tokens: &[&str]) -> Result<Resolved, String> {
    let partial = HostNounPartial::try_parse_from(tokens)
        .map_err(|e| e.to_string())?;
    partial.refine()?.resolve()
}
```

The palette checks the first token: if `"host"`, it calls `parse_host_command`; otherwise `parse_noun_command`. This mirrors the CLI's existing two-path dispatch (`SubCommand::Host(partial) => partial.refine()?.resolve()`).

**No `host` ambiguity:** The palette-local command for setting the provisioning target is `target <name>`, not `host <name>`. This avoids collision with the `host` registry noun (whose names are unconstrained strings — a peer named `status` or `repo` would be ambiguous under a shared name). `target` is also semantically clearer — it sets the provisioning target, not "the host."

For `NeedsContext`, the palette resolves ambient context from the TUI environment — repo from the active tab (`model.active_repo_identity()`), host from `HostResolution` via `resolve_host()`. Commands are pushed via `proto_commands.push(...)` on the app's command queue.

Note: code examples in this spec use simplified names for clarity. The implementation plan will map these to actual TUI integration points (`App::model`, `App::ui`, `App::proto_commands`, etc.).

### Position-aware completions

As the user types, the completion list changes based on parse position:

| Input state | Completions shown |
|-------------|-------------------|
| Empty | Noun names (`checkout`, `cr`, `issue`, `agent`, `workspace`, `repo`, `host`) + palette-local commands (`layout`, `theme`, `refresh`, `search`, `help`, ...) |
| Noun typed (`cr `) | Subjects from model — CR IDs for `cr`, branch names for `checkout`, etc. |
| Noun + subject (`cr #42 `) | Available verbs for that noun (`open`, `close`, `link-issues`) |
| Noun + subject + verb (`cr #42 link-issues `) | Verb-specific argument completions |
| Palette-local command (`layout `) | Valid arguments (`auto`, `zoom`, `right`, `below`) |

### Completion sources

Completions come from the in-memory `AppModel` snapshot. No daemon queries.

| Noun | Subject source |
|------|---------------|
| `checkout` | `providers.checkouts` — extract branch names from checkout values (keys are paths, not branch names) |
| `cr` | `providers.change_requests` — CR IDs |
| `issue` | `providers.issues` — issue IDs |
| `agent` | `providers.cloud_agents` — agent/session IDs |
| `workspace` | `providers.workspaces` — workspace refs |
| `repo` | `model.repos` — `RepoIdentity.path` (e.g., `flotilla-org/flotilla`). If two authorities share the same path, show `authority:path` to disambiguate. |
| `host` | `model.hosts` — host names |

Verb and flag completions come from the clap `Command` tree (same as the Phase 1 static completion engine).

### Context-aware filtering

The palette's available commands depend on whether the user is on a repo tab or the overview tab:

- **Repo tab**: all nouns and palette-local commands available.
- **Overview tab / no repos**: only repo-independent commands shown — `host`, `repo` (with explicit subject), palette-local commands (`target`, `layout`, `theme`, `help`, etc.). Repo-scoped nouns (`cr`, `checkout`, `issue`, `agent`, `workspace`) are hidden because both `RepoContext::Required` and `RepoContext::Inferred` commands need a repo to execute.

This filtering applies to both completion suggestions and confirm-time dispatch. If the user types a repo-scoped command on the overview tab, the palette rejects it with "switch to a repo tab first."

### Tab behavior

Tab accepts the current completion and advances the cursor, appending a space. The completion list updates for the next position.

### Confirm behavior

Enter parses the full input, resolves, and dispatches. If parsing fails, an error message appears inline (not a modal). If the command requires arguments not yet provided, the completion list highlights what's missing.

## Palette-Local Commands

Existing no-arg palette entries become argument-bearing commands. They are palette-local — they don't go through the daemon or the noun-verb registry. They are TUI settings.

| Command | Arguments | Completions |
|---------|-----------|-------------|
| `layout <name>` | `auto`, `zoom`, `right`, `below` | Fixed set |
| `theme <name>` | Available theme names | Fixed set |
| `target <name>` | `local`, known peer hostnames | From model |
| `search <query>` | Free text | None |
| `refresh` | None | — |
| `help` | None | — (toggles help overlay) |
| `quit` | None | — |
| `providers` | None | — |
| `debug` | None | — |
| `keys` | None | — |
| `select` | None | — (toggle multi-select) |
| `add-repo` | None | — (opens file picker) |

The no-arg commands work as before. The argument-bearing commands (`layout`, `theme`, `target`, `search`) show completions after a space.

## Intent Adapter

Intents become thin adapters that construct command token strings from a `WorkItem`, then resolve through the registry — where possible. Some intents have complex resolution logic that cannot yet be expressed as token construction.

### Current flow

```
key binding → Action::Dispatch(Intent) → intent.resolve(item, app) → Command → dispatch
```

### New flow (convertible intents)

```
key binding → Action::Dispatch(Intent) → intent.to_command_tokens(item, app) → Vec<String>
    → parse (noun or host path) → resolve() → Resolved → dispatch
```

### Which intents convert

| Intent | Converts? | Notes |
|--------|-----------|-------|
| `OpenChangeRequest` | Yes | Produces `cr <id> open` |
| `CloseChangeRequest` | Yes | Produces `cr <id> close` |
| `OpenIssue` | Yes | Produces `issue <id> open` |
| `ArchiveSession` | Yes | Produces `agent <id> archive` |
| `TeleportSession` | Yes | Produces `agent <id> teleport [--branch ...]` |
| `SwitchToWorkspace` | Yes | Produces `workspace <ref> select` |
| `CreateCheckout` | Yes | Produces `checkout create --branch <name> [--fresh]` |
| `GenerateBranchName` | Yes | Produces `issue <ids> suggest-branch` |
| `RemoveCheckout` | No | Two-step UI flow: dispatches `FetchCheckoutStatus` first to populate a confirmation dialog, then the dialog emits `RemoveCheckout` with `CheckoutSelector::Path` (exact path). Using branch-name-based query resolution would regress correctness — query-based checkout resolution is known to be ambiguous. |
| `CreateWorkspace` | No | Requires local/remote branching logic, `workspace create` not yet a registry noun |
| `LinkIssuesToChangeRequest` | No | Computes missing issue IDs by diffing model state — not expressible as static tokens |

Intents that cannot convert keep their current `intent.resolve(item, app)` path. They still produce a `Command` directly. This is a pragmatic split — as more nouns gain verbs (e.g., `workspace create` in phase 3), more intents can migrate.

Each convertible intent builds a token vector from the work item's data. **Host routing is handled by the ambient context model (#506):** the resolve step produces `Resolved::NeedsContext` with the appropriate `HostResolution`, and the TUI dispatch layer fills `Command.host` from the TUI's environment (provisioning target, item host, or provider host as appropriate). The intent adapter does not need to handle host routing — it just produces noun-verb tokens.

```rust
impl Intent {
    fn to_command_tokens(&self, item: &WorkItem, app: &App) -> Option<Vec<String>> {
        match self {
            Intent::OpenChangeRequest => {
                let cr_id = item.change_request_key.as_ref()?;
                Some(vec!["cr".into(), cr_id.clone(), "open".into()])
            }
            Intent::ArchiveSession => {
                let session_key = item.session_key.as_ref()?;
                Some(vec!["agent".into(), session_key.clone(), "archive".into()])
            }
            Intent::CreateCheckout => {
                let branch = item.branch.as_ref()?;
                Some(vec!["checkout".into(), "create".into(), "--branch".into(), branch.clone()])
            }
            // Non-convertible intents return None
            Intent::RemoveCheckout       // two-step flow with path-based selector
            | Intent::CreateWorkspace    // local/remote branching, no registry noun
            | Intent::LinkIssuesToChangeRequest => None,  // model diffing
            // ...
        }
    }
}
```

When `to_command_tokens` returns `None`, the dispatch falls back to the existing `intent.resolve(item, app)` path.

`Intent::is_available` and `Intent::is_allowed_for_host` stay unchanged — they still gate whether the intent is offered.

The action menu continues to use the current intent resolution path for now. It will evolve into a contextual palette view in a later phase.

### What this validates

Every converted intent round-trips through `parse_noun_command` → `resolve()`, exercising the same path the palette uses. Bugs in noun parsing surface through normal TUI usage, not just CLI testing.

## Command Echo

When a TUI action fires via key binding (not the palette), the status bar briefly shows the resolved command text.

| User action | Echo |
|-------------|------|
| Press `p` on a CR | `cr #42 open` |
| Press `Enter` on a workspace | `workspace feat-ws select` |
| Press `Enter` on a session | `agent claude-1 teleport` |

For non-convertible intents (`RemoveCheckout`, `CreateWorkspace`, `LinkIssuesToChangeRequest`), no echo is shown — they don't have a clean command string representation yet.

### Status bar layout

The status bar is reorganized into four sections:

```
| command echo / pre-fill | key hints | errors/status/actions | layout/host prefs |
```

1. **Command echo / pre-fill** (left) — transient command text from key-binding actions, or the pre-fill preview showing what `?` would open. This is a new `command_echo` field on `UiState`, separate from `status_message`. Cleared on next key press.

2. **Key hints** — existing key binding hints (unchanged).

3. **Errors / status / actions** — the existing `status_message` field, used for errors, step progress, and provider failures. No change to its semantics.

4. **Layout / host prefs** (right) — existing layout and host display (unchanged).

This separation means command echo never collides with error messages. They are different fields rendered in different sections.

### Pre-fill preview

When a work item is selected, the command echo section shows the pre-fill preview with `?` highlighted:

```
| /cr #42 ? | d Del  p PR  / Cmd | | zoom  local |
```

The `?` is visually highlighted (e.g., bold or contrasting color) to hint "press `?` to act on this." When no selection exists:

```
| | d Del  p PR  / Cmd  h Help | | zoom  local |
```

### Implementation

After `intent.to_command_tokens()` produces tokens, join them into a display string and set `app.ui_state.command_echo`. Cleared on next key press.

## Key Binding Changes

| Binding | Old | New |
|---------|-----|-----|
| `Shared ?` | `ToggleHelp` | Remove |
| `Normal ?` | `ToggleHelp` (hint "Help") | `OpenContextualPalette` (hint "Ctx") |
| `Help ?` | `ToggleHelp` (hint "Close") | Remove (use `h` or `esc` to close) |
| `Normal h` | `CycleHost` | `ToggleHelp` (hint "Help") |
| `Help h` | (unbound) | `ToggleHelp` (hint "Close") |
| `Esc` (Normal mode) | (quit chain) | Clear table selection (before quit in chain) |

`CycleHost` has no direct replacement key — host/target selection moves to the palette (`target <name>`).

### Esc behavior

`Esc` in modal contexts (palette, action menu, confirm dialogs) still dismisses the modal. The current Normal-mode `Esc` handler has a priority chain in `repo_page.rs`: cancel in-flight command → clear search → hide providers → hide archived → clear multi-select → quit. "Clear table selection" inserts before "quit" in this chain — it is the last thing tried before quitting. This means:

1. If search is active, `Esc` clears search (existing behavior)
2. If multi-select is active, `Esc` clears multi-select (existing behavior)
3. If a row is selected, `Esc` clears the selection (new)
4. If nothing is selected, `Esc` quits (existing behavior)

## Dispatch in TUI

The palette and intent adapter both produce `Resolved` values. The TUI needs a dispatch function analogous to `main.rs::dispatch()`:

```rust
/// Repo context comes from the active tab, not from the model's last-selected repo.
/// Returns None on the overview tab, Some(identity) on repo tabs.
/// This is the source of truth for palette dispatch — not active_repo_identity_opt(),
/// which returns the last-selected repo even on the overview tab.
fn active_tab_repo(app: &App) -> Option<RepoSelector> {
    if app.ui.is_config {
        return None; // overview tab — no repo context
    }
    Some(RepoSelector::Identity(app.model.active_repo_identity().clone()))
}

fn tui_dispatch(resolved: Resolved, item: Option<&WorkItem>, app: &mut App) -> Result<(), String> {
    match resolved {
        Resolved::Ready(cmd) => {
            app.push_command(cmd);
            Ok(())
        }
        Resolved::NeedsContext { mut command, repo, host } => {
            let tab_repo = active_tab_repo(app);
            match repo {
                RepoContext::Required => {
                    let repo_sel = tab_repo
                        .ok_or("no active repo — switch to a repo tab first")?;
                    command.context_repo = Some(repo_sel.clone());
                    fill_repo_sentinels(&mut command.action, repo_sel);
                }
                RepoContext::Inferred => {
                    command.context_repo = tab_repo;
                }
            }
            command.host = resolve_host(host, item, app);
            app.push_command(command);
            Ok(())
        }
    }
}

fn resolve_host(resolution: HostResolution, item: Option<&WorkItem>, app: &App) -> Option<HostName> {
    match resolution {
        HostResolution::Local => None,
        HostResolution::ProvisioningTarget => app.ui.target_host.clone(),
        HostResolution::SubjectHost => {
            // item's host if different from local host
            item.and_then(|i| app.item_execution_host(i))
        }
        HostResolution::ProviderHost => {
            if app.active_repo_is_remote_only() {
                item.and_then(|i| app.item_execution_host(i))
            } else {
                None
            }
        }
    }
}
```

This replaces the current `resolve_and_push` for intent-driven actions (for convertible intents) and the six TUI command builders. Special handling for confirmation dialogs (delete confirm, close confirm, branch input) stays — those are UI flow concerns that wrap around the dispatch.

## Crate Boundaries

| Change | Crate |
|--------|-------|
| Palette widget overhaul (parsing, completions, pre-fill) | `flotilla-tui` |
| Completion source trait + model-backed sources | `flotilla-tui` (sources query `AppModel`) |
| Intent adapter (`to_command_tokens`) | `flotilla-tui` |
| Command echo in status bar | `flotilla-tui` |
| Key binding changes (?, h, Esc) | `flotilla-tui` |
| Palette-local command definitions | `flotilla-tui` |
| `Resolved::NeedsContext` + `HostResolution` + `RepoContext` | `flotilla-commands` (already landed) |
| `Esc` clears selection | `flotilla-tui` |

`flotilla-commands` gains `parse_noun_command()` and `parse_host_command()` — convenience wrappers for parsing token sequences. `HostResolution`, `RepoContext`, and `Resolved::NeedsContext` are already landed. No changes to `flotilla-core`, `flotilla-protocol`, or `flotilla-daemon`.

## Testing

### Palette parsing tests

Verify that typed input resolves to the correct `Resolved` variant:

```rust
#[test]
fn palette_parses_cr_close() {
    let resolved = parse_palette_input("cr #42 close");
    // cr close has ProviderHost resolution → NeedsContext
    assert!(matches!(resolved, Ok(Resolved::NeedsContext { command, host: HostResolution::ProviderHost, .. })
        if matches!(command.action, CommandAction::CloseChangeRequest { .. })));
}
```

### Pre-fill tests

Verify work item → pre-fill string mapping:

```rust
#[test]
fn prefill_from_cr_item() {
    let item = work_item_with_cr("#42");
    let prefill = palette_prefill(&item);
    assert_eq!(prefill, "cr #42 ");
}
```

### Intent adapter round-trip tests

Verify that intent → tokens → parse → resolve produces the same `Command` as the old `intent.resolve()`:

```rust
#[test]
fn intent_round_trips_through_registry() {
    let item = work_item_with_cr("#42");
    let tokens = Intent::OpenChangeRequest.to_command_tokens(&item, &app).unwrap();
    let noun = parse_noun_command(&tokens).unwrap();
    let resolved = noun.resolve().unwrap();
    // cr open has ProviderHost resolution → NeedsContext
    assert!(matches!(resolved, Resolved::NeedsContext { command, host: HostResolution::ProviderHost, .. }
        if matches!(command.action, CommandAction::OpenChangeRequest { .. })));
}

#[test]
fn non_convertible_intent_returns_none() {
    let item = work_item_with_checkout("my-feature");
    assert!(Intent::RemoveCheckout.to_command_tokens(&item, &app).is_none());
}
```

### Completion tests

Verify position-aware completions from model data:

```rust
#[test]
fn completes_cr_subjects_from_model() {
    let model = model_with_crs(vec!["#42", "#99"]);
    let completions = palette_completions("cr ", &model);
    assert!(completions.iter().any(|c| c.value == "#42"));
    assert!(completions.iter().any(|c| c.value == "#99"));
}
```

### Command echo tests

Verify that key-binding actions produce the expected echo string:

```rust
#[test]
fn echo_for_open_pr_keybinding() {
    let item = work_item_with_cr("#42");
    let echo = Intent::OpenChangeRequest.command_echo(&item, &app);
    assert_eq!(echo, Some("cr #42 open".to_string()));
}

#[test]
fn no_echo_for_non_convertible_intent() {
    let item = work_item_with_checkout("my-feature");
    let echo = Intent::RemoveCheckout.command_echo(&item, &app);
    assert_eq!(echo, None);
}
```

## Scope

### Delivers

- Palette parses noun-verb commands via `parse_noun_command` → `resolve()` → dispatch
- Position-aware completions from in-memory model
- `?` opens contextual palette (pre-filled from selection)
- `/` opens global palette (empty)
- `Esc` clears table selection in Normal mode
- Help moves to `h`
- Palette-local commands gain argument support (`layout <name>`, `theme <name>`, `target <name>`)
- Intents construct command token strings, resolve through registry
- Command echo in status bar on key-binding actions
- Status bar shows pre-fill preview when work item selected

### Defers

- CLI dynamic completions (daemon-queried subjects for shell completion)
- Action menu changes (stays as-is; future: contextual palette view)
- Plan composition / stepper (phase 3)
- `workspace create` (needs step executor work)
- Richer "partial" command representation (filled/missing/bound values)

## Open Questions

- Pre-fill priority when a work item has multiple signals (CR + checkout + agent). Proposed: CR > checkout > issue > agent > workspace, but may need tuning.
- Whether palette-local commands should eventually become registry nouns or stay TUI-local.
