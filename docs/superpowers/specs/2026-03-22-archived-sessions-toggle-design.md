# Archived/Expired Sessions Toggle — Design

**Issue:** #245
**Date:** 2026-03-22

## Problem

Claude and Cursor providers filter out archived/expired sessions before they reach the UI. Users cannot see past sessions or distinguish their end-of-life state.

## Design

Two coordinated changes: providers stop filtering, the TUI adds a visibility toggle.

### Provider changes

**Claude** (`coding_agent/claude.rs`): Remove the `.filter(|s| s.session_status != "archived")` in `fetch_sessions_inner`. The inline status match at line 309 already maps `"archived"` → `SessionStatus::Archived`; sessions just never reach that code path today because they are filtered first. Update the existing provider test (`fetch_sessions_inner_filters_archived_sorts_and_sends_auth_header`) to expect all sessions including archived ones, and rename it to reflect the new behaviour.

**Cursor** (`coding_agent/cursor.rs`): Remove the `.filter(|a| a.session_status() != SessionStatus::Expired)` in `list_sessions`.

After these changes, all sessions flow through correlation and appear in `ProviderData.sessions` regardless of status.

### TUI changes

**Toggle state:** Add `show_archived: bool` (default `false`) to `RepoPage`. Per-tab, session-only (not persisted).

**Key binding:** `Action::ToggleArchived`, bound to `u` in Normal mode. Follows the `ToggleProviders` / `c` pattern.

**Filtering:** In `RepoPage::reconcile_if_changed`, after `group_work_items` returns and before passing to `update_items`, filter the `GroupedWorkItems` when `show_archived` is false. The filter removes `GroupEntry::Item` entries where `item.kind` is `WorkItemKind::Session` and the looked-up `CloudAgentSession.status` is `Archived` or `Expired`. `WorkItemKind::Agent` items (local CLI agents) are never filtered — they don't have a `SessionStatus`.

After filtering entries, rebuild `selectable_indices` from scratch (iterate `table_entries`, collect indices of remaining `GroupEntry::Item` entries). Remove orphaned section headers — any `GroupEntry::Header` followed immediately by another header or the end of the list.

Items correlated with other non-session kinds (e.g., a checkout that happens to have an archived session on the same branch) have `kind: WorkItemKind::Checkout`, not `Session`, so they remain visible naturally.

A helper method `GroupedWorkItems::filter_archived_sessions(providers: &ProviderData)` encapsulates this logic and returns a new `GroupedWorkItems`.

**Rendering — icons:** Distinguish expired from archived in `ui_helpers.rs`:
- `SessionStatus::Archived` → `○` (open circle, current)
- `SessionStatus::Expired` → `⊘` (circle with stroke)

Update `work_item_icon` to use the same distinction. Both use the existing `theme.session` colour.

**Rendering — dimming:** When `show_archived` is true, archived/expired session rows render with a dimmed style. In `work_item_table.rs`, the row style for these items uses `theme.surface1` (or similar muted colour) instead of the default foreground.

**Status bar:** `StatusFragment` from `RepoPage` adds an `"ARCHIVED"` label when the toggle is on. Priority order: `show_providers` > `active_search_query` > `show_archived` > `multi_selected` > none. Only one status label shows at a time, matching the existing pattern.

**Dismiss chain:** `Esc` dismiss in `RepoPage` currently clears search → providers → multi-select → quit. Insert `show_archived` after providers: search → providers → archived → multi-select → quit.

### What stays unchanged

- Correlation: archived sessions still correlate via `CorrelationKey::Branch` and `SessionRef`. A work item that merges an archived session with a checkout shows the checkout normally — only standalone session-only items get hidden.
- `group_work_items` in core: no changes. Filtering is TUI-side.
- Config persistence: none for now. Future "gear page" will control this.

## Files

| File | Change |
|------|--------|
| `crates/flotilla-core/src/providers/coding_agent/claude.rs` | Remove archived filter, update test |
| `crates/flotilla-core/src/providers/coding_agent/cursor.rs` | Remove expired filter |
| `crates/flotilla-tui/src/keymap.rs` | Add `Action::ToggleArchived`, config string |
| `crates/flotilla-tui/src/binding_table.rs` | Add `(Normal, "u", ToggleArchived)` binding |
| `crates/flotilla-tui/src/widgets/repo_page.rs` | Add `show_archived` field, handle toggle action, filter in `reconcile_if_changed`, status fragment, dismiss chain |
| `crates/flotilla-tui/src/widgets/work_item_table.rs` | Dimmed style for archived/expired rows |
| `crates/flotilla-tui/src/ui_helpers.rs` | `⊘` for expired in `session_status_display` and `work_item_icon` |
| `crates/flotilla-core/src/data.rs` | Add `GroupedWorkItems::filter_archived_sessions` helper |
