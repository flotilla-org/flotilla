# Provider Attribution Throughout the UI

**Issues:** #173, #187
**Date:** 2026-03-11

## Problem

When multiple providers contribute data in the same category (e.g., Claude and Cursor as coding agents), the UI does not distinguish them. The session preview shows "Session: {title}" with no provider name. The abbreviation column shows the same `Ses` for every coding agent. Provider identity is lost as data flows from provider trait objects through `ProviderData` into protocol `WorkItem`s.

## Data Flow Fix: Threading Provider Identity

The root cause is that provider identity is not preserved in the protocol data types. The `CorrelatedItem.provider_name` field in the correlation engine holds category names (`"session"`, `"checkout"`), not actual provider names (`"claude"`, `"github"`). We sidestep this by adding provider names directly to the protocol data structs, so identity is available wherever the data is used. The correlation engine values do not need to change.

### Add `provider_name` to Protocol Data Types

Add `provider_name: String` to these structs in `flotilla-protocol/src/provider_data.rs`:

- `CloudAgentSession` -- populated by each coding agent provider (e.g., `"claude"`, `"cursor"`, `"codex"`)
- `ChangeRequest` -- populated by code review providers (e.g., `"github"`)
- `Issue` -- populated by issue tracker providers (e.g., `"github"`)

Each provider implementation sets `provider_name` when constructing these structs during data collection. The field uses `#[serde(default)]` for snapshot compatibility.

**Fallback for empty `provider_name`:** When `provider_name` is empty (old snapshots or missing data), the preview and table fall back to the generic category noun (e.g., "Agent" instead of "Claude Agent", empty source column cell). No crash, no panic -- just graceful degradation.

Checkouts do not need `provider_name` -- their source is always the local hostname, obtained via `gethostname()` at the point of use.

### Add `source` to Protocol `WorkItem`

Add `source: Option<String>` to `WorkItem` in `flotilla-protocol/src/snapshot.rs`, with `#[serde(default)]`.

Populated during the correlation-to-WorkItem conversion in `convert.rs` / `data.rs`:

| WorkItem kind | Source value |
|---------------|-------------|
| Checkout | Local hostname via `gethostname()` |
| Session | `session.provider_name` (looked up via session key in `ProviderData`) |
| ChangeRequest | `change_request.provider_name` (looked up via CR key in `ProviderData`) |
| Issue | `issue.provider_name` (looked up via issue key in `ProviderData`) |
| RemoteBranch | `"git"` (hardcoded for now; only one VCS provider exists) |

For correlated work items with multiple linked providers (e.g., a Checkout anchored with a linked Claude session and a GitHub PR), `source` reflects the **anchor** item only. Linked items' providers are accessible via their respective data lookups when needed (e.g., for preview rendering).

No separate `session_provider` field is needed. When the preview renderer looks up `providers.sessions.get(session_key)`, it reads `provider_name` directly from the `CloudAgentSession` struct.

## Table: Source Column

Add a `Source` column positioned before the existing `Path` column.

Content comes from `WorkItem.source`. Repeated values within a section are elided for visual clarity.

The column is auto-sized and compact -- these are short strings like `claude`, `github`, or a hostname.

This adds one column to the table, requiring updates to `build_header_row` (currently 10 cells, becomes 11) and row construction in `build_item_row`.

## Table: Abbreviation Rename

Change the `CloudAgentService` trait default: `abbreviation()` returns `"Agt"` instead of `"Ses"`, `item_noun()` returns `"agent"` instead of `"session"`.

Existing provider overrides are preserved -- Codex already overrides to `"Cdx"` / `"task"` and keeps those values. Cursor currently inherits the default, so it picks up `"Agt"` / `"agent"` automatically.

## Preview Panel

All preview types use `"{Provider} {Noun}: {title}"` format:

- **Sessions:** `"Claude Agent: Refactoring the parser"` -- `provider_name` from `CloudAgentSession`, title-cased
- **Change Requests:** `"GitHub PR: Fix parsing bug"` -- `provider_name` from `ChangeRequest`, title-cased
- **Issues:** `"GitHub Issue: Login broken"` -- `provider_name` from `Issue`, title-cased
- **Checkouts:** No change needed (path is self-evident)

When `provider_name` is empty, fall back to the generic noun alone (e.g., just `"Agent: title"`).

Session preview adds an `Id:` line showing the session identifier (the `session_key` from the `WorkItem`):

```
Claude Agent: Refactoring the parser
Id: ses_abc123
Status: Running
Model: claude-sonnet-4-20250514
Updated: 2025-01-15
```

The preview renderer already looks up data via `providers.sessions.get(key)` etc., so it can read `provider_name` directly from the struct -- no additional data threading needed.

## Logging

Add a `provider` structured tracing field to log entries in provider implementations:

```rust
debug!(provider = "claude", "Fetched 12 sessions");
debug!(provider = "github", "Fetched 5 PRs");
```

The `MessageVisitor` in `event_log.rs` currently only captures the `message` field. Extend it to also capture a `provider` field when present, and prepend `[provider]` to the displayed message.

## Health/Status Display

Already implemented as desired in `render_repo_providers` (`ui.rs:221-290`). First provider on the category line, subsequent providers aligned beneath. No changes needed.

## Tests

Adding `provider_name` to `CloudAgentSession`, `ChangeRequest`, and `Issue`, plus `source` to `WorkItem`, will require updating all test sites that construct these structs. Key locations:

- `provider_data.rs` tests (roundtrip tests for sessions, CRs, issues)
- `snapshot.rs` tests (WorkItem construction)
- `convert.rs` tests (protocol conversion)
- Provider-specific tests in `claude.rs`, `codex.rs`, `cursor.rs`
- `data.rs` correlation tests

## Files Involved

| File | Change |
|------|--------|
| `crates/flotilla-protocol/src/provider_data.rs` | Add `provider_name` to `CloudAgentSession`, `ChangeRequest`, `Issue` |
| `crates/flotilla-protocol/src/snapshot.rs` | Add `source` to `WorkItem` |
| `crates/flotilla-core/src/data.rs` | Populate `WorkItem.source` during correlation conversion |
| `crates/flotilla-core/src/convert.rs` | Thread `source` through protocol conversion |
| `crates/flotilla-core/src/providers/coding_agent/mod.rs` | Change default `abbreviation()` to `"Agt"`, `item_noun()` to `"agent"` |
| `crates/flotilla-core/src/providers/coding_agent/claude.rs` | Set `provider_name` on sessions |
| `crates/flotilla-core/src/providers/code_review/github.rs` | Set `provider_name` on change requests |
| `crates/flotilla-core/src/providers/issue_tracker/github.rs` | Set `provider_name` on issues |
| `crates/flotilla-tui/src/ui.rs` | Add Source column, update preview rendering |
| `crates/flotilla-tui/src/event_log.rs` | Extend `MessageVisitor` to capture `provider` field |
| Provider implementations | Add `provider` field to log entries |

## Out of Scope

- Kitty graphics protocol icons (general facility, deferred)
- Glyph-based provider indicators with legend/hover (general facility, deferred)
- Table split into separate checkout vs. work-item tables (#198)
- Host column showing remote hosts (#33)
- Web links from session IDs
- "Code review" vs "change request" terminology unification (separate issue to file)
