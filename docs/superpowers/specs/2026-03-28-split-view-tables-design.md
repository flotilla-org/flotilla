# Split-View Tables — Phase 1

**Issue:** [#198](https://github.com/flotilla-org/flotilla/issues/198)
**Branch:** `feat/split-view-tables`
**Date:** 2026-03-28

## Problem

The repo page renders all work items in a single table with one fixed column set. Every row — checkout, PR, cloud agent session, issue — shares the same 11 columns. Most columns are irrelevant to most row kinds: issues have no Git status, checkouts have no Labels, cloud agents have no Path. The result is a sparse, cluttered table.

The immediate driver: issues should not flow through correlation just to appear in the table. They need their own section with purpose-built columns (ID, Title, Labels, Linked PR). This forces us to build per-section column infrastructure, which benefits every section.

## Approach

Build a generic `SectionTable<T>` widget parameterized over the row type. Compose multiple `SectionTable<WorkItem>` instances into a `SplitTable` that replaces the current `WorkItemTable`. Each section gets its own column definitions tuned to its content. Issues become the first section with a distinct column layout.

The generic row type parameter means a future protocol change can introduce a dedicated `IssueRow` type without modifying the widget infrastructure.

## Design

### `SectionTable<T>` — generic table widget

A new widget in `flotilla-tui/src/widgets/section_table.rs`, generic over row type `T`:

```rust
struct ColumnDef<T> {
    header: String,
    width: Constraint,
    extract: Box<dyn Fn(&T, &RenderCtx) -> Cell<'static>>,
}

struct SectionTable<T> {
    columns: Vec<ColumnDef<T>>,
    items: Vec<T>,
    table_state: TableState,
    selected_idx: Option<usize>,
    header_label: String,
}
```

`RenderCtx` bundles contextual dependencies that extractors need: theme, providers, column widths, repo root. Extractor closures stay `Fn(&T, &RenderCtx) -> Cell` rather than accumulating positional arguments.

The widget handles rendering (header row, item rows, scroll state) and selection (next, prev, select-by-identity). It does not render section divider headers — that responsibility belongs to the composing widget.

### `SplitTable` — section composition

Replaces `WorkItemTable` as the widget owned by `RepoPage`:

```rust
struct SplitTable {
    sections: Vec<SectionTable<WorkItem>>,
    active_section: usize,
    table_area: Rect,
    gear_area: Option<Rect>,
}
```

**Navigation.** `j`/`k` moves within the active section. Moving past the last item of a section advances focus to the first item of the next non-empty section, and vice versa. Mouse clicks resolve to section and row via hit-testing. The result is a seamless single-list feel, with independent sections underneath.

**Selection identity.** `SplitTable` exposes `selected_work_item() -> Option<&WorkItem>` — the same API as today. `RepoPage`, `PreviewPanel`, and action dispatch remain unchanged.

**Rendering.** `SplitTable` renders each section's divider header (`── Checkouts ──`), then delegates to the `SectionTable` for column headers and item rows. Sections stack vertically. Empty sections are skipped entirely.

**Height allocation.** Proportional to item count, with a minimum of header + 1 row for non-empty sections. The active section receives any remaining space, so a focused Issues section with 30 items gets the lion's share when Checkouts has 3.

### Data flow

`group_work_items()` changes its return type from the flat `GroupedWorkItems` (interleaved headers and items) to structured sections:

```rust
struct SectionData {
    kind: SectionKind,
    label: String,
    items: Vec<WorkItem>,  // already sorted
}

enum SectionKind {
    Checkouts,
    AttachableSets,
    CloudAgents,
    ChangeRequests,
    RemoteBranches,
    Issues,
}
```

The sorting logic per section is identical to today — it collects into separate `SectionData` structs instead of interleaving into a single `Vec<GroupEntry>`.

`SplitTable::update_sections(sections: Vec<SectionData>)` distributes items to each `SectionTable`. Existing sections update in place (preserving selection by identity). Sections that receive no items are dropped. Column definitions are created per `SectionKind` via `columns_for_section()`.

`RepoPage::rebuild_table` becomes:

```rust
fn rebuild_table(&mut self, data: &RepoData) {
    let sections = group_work_items_split(
        &data.work_items, &data.providers, &labels, &data.path,
    );
    self.split_table.update_sections(sections);
}
```

### Column definitions per section

**Checkouts** — close to today's full layout:

| Header | Width | Source |
|--------|-------|--------|
| (icon) | Length(3) | `work_item_icon` |
| Source | Length(10) | host/provider, deduped |
| Path | Fill(1) | shortened checkout path |
| Description | Fill(2) | PR title or branch |
| Branch | Fill(1) | branch name |
| WT | Length(3) | main/worktree indicator |
| WS | Length(3) | workspace indicator |
| PR | Length(4) | linked PR + status icon |
| SS | Length(4) | session/agent status |
| Issues | Length(6) | linked issue IDs |
| Git | Length(5) | working tree status |

**Cloud Agents** — drops checkout-specific columns:

| Header | Width | Source |
|--------|-------|--------|
| (icon) | Length(3) | session/agent icon |
| Source | Length(10) | provider name |
| Key | Fill(1) | session key |
| Description | Fill(2) | session title |
| Branch | Fill(1) | branch if any |
| Status | Length(8) | session/agent status |

**Change Requests** — PR-focused:

| Header | Width | Source |
|--------|-------|--------|
| (icon) | Length(3) | PR icon |
| PR# | Length(6) | number + status icon |
| Title | Fill(2) | PR title |
| Branch | Fill(1) | branch |
| State | Length(8) | open/merged/draft |
| Issues | Length(8) | linked issue IDs |

**Issues** — purpose-built:

| Header | Width | Source |
|--------|-------|--------|
| (icon) | Length(3) | issue icon |
| ID | Length(6) | `#123` |
| Title | Fill(2) | issue title |
| Labels | Fill(1) | comma-joined labels |
| PR | Length(6) | linked PR if any |

**Attachable Sets** and **Remote Branches** get simple layouts appropriate to their content.

### Issues behavior

The Issues section keeps current correlation output: standalone issues (not linked to any PR or checkout) appear as rows. Linked issues continue to show as a column/badge on their correlated work item in the Checkouts or Change Requests section.

Future work adds a view toggle to show all issues in the Issues section, filtering out those already visible in higher sections.

## Migration

- `GroupedWorkItems`, `GroupEntry`, and `SectionHeader` become dead code once `SplitTable` replaces `WorkItemTable`. Remove them.
- `build_item_row()` and `build_header_row()` decompose into per-column extractor closures. The logic moves; it is not duplicated.
- `RepoPage`'s public API (selected item, action dispatch, multi-select, pending actions) stays unchanged. This is an internal refactor of table rendering.
- Preview panel, action menu, and key handlers operate on `WorkItemIdentity`, which `SplitTable` still exposes.

## Testing

- **`SectionTable<T>` unit tests** with a simple test struct (not `WorkItem`): selection preservation, navigation, empty table handling. Proves the generic works independently of domain types.
- **`columns_for_section()` tests** per section kind: column count and header verification.
- **`SplitTable` integration tests**: cross-section navigation, selection identity preservation across data updates — migrated from existing `WorkItemTable` tests.
- **Snapshot tests** for rendered output of each section kind with representative items.

## Out of scope

- LogIdx/VisIdx remap layer for per-section sort/filter (Phase 3 of #198).
- Dedicated `IssueRow` protocol type (future protocol change; the generic infrastructure supports it).
- View toggle for linked-vs-standalone issues in the Issues section.
- Per-section sort/filter UI.
