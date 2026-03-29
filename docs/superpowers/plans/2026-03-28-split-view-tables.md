# Split-View Tables Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the monolithic work-item table with per-section tables, each with its own column set, starting with a dedicated Issues section.

**Architecture:** A generic `SectionTable<T>` widget renders rows of any type with configurable columns. A `SplitTable` composes multiple `SectionTable<WorkItem>` instances — one per section kind (Checkouts, Cloud Agents, Change Requests, Issues, etc.) — replacing `WorkItemTable`. The data layer changes from a flat interleaved list (`GroupedWorkItems`) to structured `Vec<SectionData>`.

**Tech Stack:** Rust, ratatui, flotilla-core data layer, flotilla-tui widget tree

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/flotilla-core/src/data.rs` | Modify | Add `SectionKind`, `SectionData`, `group_work_items_split()`; keep old types until migration complete |
| `crates/flotilla-core/src/data/tests.rs` | Modify | Add tests for `group_work_items_split()` |
| `crates/flotilla-tui/src/widgets/section_table.rs` | Create | Generic `SectionTable<T>`, `ColumnDef<T>`, `RenderCtx` |
| `crates/flotilla-tui/src/widgets/split_table.rs` | Create | `SplitTable` composing multiple `SectionTable<WorkItem>`, cross-section navigation, height allocation |
| `crates/flotilla-tui/src/widgets/columns.rs` | Create | `columns_for_section()` returning `Vec<ColumnDef<WorkItem>>` per `SectionKind` |
| `crates/flotilla-tui/src/widgets/mod.rs` | Modify | Add module declarations for new files |
| `crates/flotilla-tui/src/widgets/repo_page.rs` | Modify | Replace `WorkItemTable` with `SplitTable`, update `rebuild_table`, action dispatch, mouse handling |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Modify | Update `GroupEntry` access patterns to use `SplitTable` API |
| `crates/flotilla-tui/src/app/mod.rs` | Modify | Update `GroupEntry` access to use `SplitTable` API |
| `crates/flotilla-tui/src/app/test_support.rs` | Modify | Replace `grouped_items()` / `set_active_table_view()` helpers |
| `crates/flotilla-tui/src/app/navigation/tests.rs` | Modify | Update to use new table API |
| `crates/flotilla-tui/tests/support/high_fidelity.rs` | Modify | Update `GroupEntry` references |
| `crates/flotilla-tui/src/widgets/work_item_table.rs` | Delete | Replaced by `section_table.rs` + `split_table.rs` |

---

### Task 1: Add `SectionKind` and `SectionData` to the data layer

**Files:**
- Modify: `crates/flotilla-core/src/data.rs`

This task adds the new data types alongside the existing ones. Nothing consumes them yet.

- [ ] **Step 1: Add `SectionKind` enum and `SectionData` struct**

In `crates/flotilla-core/src/data.rs`, after the `SectionLabels` impl (after line 339), add:

```rust
/// Identifies a table section by the kind of work items it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    Checkouts,
    AttachableSets,
    CloudAgents,
    ChangeRequests,
    RemoteBranches,
    Issues,
}

/// A single section's worth of sorted work items, ready for display.
#[derive(Debug, Clone)]
pub struct SectionData {
    pub kind: SectionKind,
    pub label: String,
    pub items: Vec<flotilla_protocol::WorkItem>,
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-core --locked`
Expected: compiles with no errors (types are defined but unused — allow dead_code warning for now)

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/data.rs
git commit -m "feat: add SectionKind and SectionData types for split-view tables"
```

---

### Task 2: Implement `group_work_items_split()`

**Files:**
- Modify: `crates/flotilla-core/src/data.rs`
- Test: `crates/flotilla-core/src/data/tests.rs`

- [ ] **Step 1: Write the failing test**

In `crates/flotilla-core/src/data/tests.rs`, add:

```rust
#[test]
fn group_work_items_split_produces_correct_sections() {
    use crate::data::{group_work_items_split, SectionKind};

    let labels = default_labels();

    // Build a mix of work items across several kinds
    let co = test_checkout_work_item("feat/a", "/tmp/a", false);
    let pr = test_pr_work_item("42", "Fix bug", "feat/a");
    let issue = test_issue_work_item("10", "Crash on start");
    let session = test_session_work_item_basic("s1", "Debugging session");
    let remote = test_remote_branch_work_item("origin/feat/b");

    let items = vec![co, pr, issue, session, remote];
    let providers = ProviderData::default();
    let sections = group_work_items_split(&items, &providers, &labels, Path::new("/tmp"));

    let kinds: Vec<SectionKind> = sections.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&SectionKind::Checkouts));
    assert!(kinds.contains(&SectionKind::ChangeRequests));
    assert!(kinds.contains(&SectionKind::Issues));
    assert!(kinds.contains(&SectionKind::CloudAgents));
    assert!(kinds.contains(&SectionKind::RemoteBranches));

    // Each section contains only its kind
    for section in &sections {
        for item in &section.items {
            match section.kind {
                SectionKind::Checkouts => assert_eq!(item.kind, WorkItemKind::Checkout),
                SectionKind::CloudAgents => assert!(
                    item.kind == WorkItemKind::Session || item.kind == WorkItemKind::Agent
                ),
                SectionKind::ChangeRequests => assert_eq!(item.kind, WorkItemKind::ChangeRequest),
                SectionKind::Issues => assert_eq!(item.kind, WorkItemKind::Issue),
                SectionKind::RemoteBranches => assert_eq!(item.kind, WorkItemKind::RemoteBranch),
                SectionKind::AttachableSets => assert_eq!(item.kind, WorkItemKind::AttachableSet),
            }
        }
    }

    // Empty sections are omitted
    assert!(!kinds.contains(&SectionKind::AttachableSets));
}

#[test]
fn group_work_items_split_empty_input() {
    use crate::data::group_work_items_split;
    let labels = default_labels();
    let providers = ProviderData::default();
    let sections = group_work_items_split(&[], &providers, &labels, Path::new("/tmp"));
    assert!(sections.is_empty());
}
```

Note: you may need to add test helper functions for work item kinds not currently covered by existing helpers (like `test_pr_work_item`, `test_remote_branch_work_item`, `test_session_work_item_basic`). Model them on the existing `test_session_work_item()` at line ~1080 in `data/tests.rs` — build a `flotilla_protocol::WorkItem` with the correct `kind` and relevant fields filled in.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core --locked group_work_items_split`
Expected: compilation error — `group_work_items_split` not defined

- [ ] **Step 3: Implement `group_work_items_split()`**

In `crates/flotilla-core/src/data.rs`, after `group_work_items()` (after line 797), add the new function. It reuses the same per-section sorting logic as `group_work_items()` but outputs `Vec<SectionData>` instead of `GroupedWorkItems`:

```rust
/// Group work items into per-section sorted lists.
///
/// Each non-empty section becomes a `SectionData` with its items already sorted.
/// Empty sections are omitted. Ordering of sections matches the display order:
/// Checkouts, AttachableSets, CloudAgents, ChangeRequests, RemoteBranches, Issues.
pub fn group_work_items_split(
    work_items: &[flotilla_protocol::WorkItem],
    providers: &ProviderData,
    labels: &SectionLabels,
    repo_root: &Path,
) -> Vec<SectionData> {
    let mut checkout_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut attachable_set_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut session_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut pr_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut remote_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut issue_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();

    for item in work_items {
        match item.kind {
            WorkItemKind::Checkout => checkout_items.push(item),
            WorkItemKind::AttachableSet => attachable_set_items.push(item),
            WorkItemKind::Session | WorkItemKind::Agent => session_items.push(item),
            WorkItemKind::ChangeRequest => pr_items.push(item),
            WorkItemKind::RemoteBranch => remote_items.push(item),
            WorkItemKind::Issue => issue_items.push(item),
        }
    }

    let mut sections = Vec::new();

    // Checkouts — group by host, then main first, then proximity, then path
    checkout_items.sort_by_cached_key(|item| {
        let host_name = item.host.to_string();
        let main_tier = u8::from(!item.is_main_checkout);
        let key = item.checkout_key();
        let proximity_tier = key.map(|p| checkout_sort_tier(&p.path, repo_root)).unwrap_or(1);
        let path_key = key.map(|p| p.path.to_path_buf());
        (host_name, main_tier, proximity_tier, path_key)
    });
    if !checkout_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::Checkouts,
            label: labels.checkouts.clone(),
            items: checkout_items.iter().map(|i| (*i).clone()).collect(),
        });
    }

    // Attachable sets — sorted by description
    attachable_set_items.sort_by(|a, b| a.description.cmp(&b.description));
    if !attachable_set_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::AttachableSets,
            label: "Attachable Sets".into(),
            items: attachable_set_items.iter().map(|i| (*i).clone()).collect(),
        });
    }

    // Cloud Agents — grouped by provider, then sorted by updated_at descending
    session_items.sort_by(|a, b| {
        let a_ses = a.session_key.as_deref().and_then(|k| providers.sessions.get(k));
        let b_ses = b.session_key.as_deref().and_then(|k| providers.sessions.get(k));
        let a_provider = a_ses.map(|s| s.provider_name.as_str()).unwrap_or("");
        let b_provider = b_ses.map(|s| s.provider_name.as_str()).unwrap_or("");
        a_provider.cmp(b_provider).then_with(|| {
            let a_time = a_ses.and_then(|s| s.updated_at.as_deref());
            let b_time = b_ses.and_then(|s| s.updated_at.as_deref());
            b_time.cmp(&a_time)
        })
    });
    if !session_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::CloudAgents,
            label: labels.sessions.clone(),
            items: session_items.iter().map(|i| (*i).clone()).collect(),
        });
    }

    // Change Requests — sorted by id descending
    pr_items.sort_by(|a, b| {
        let a_num = a.change_request_key.as_deref().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.change_request_key.as_deref().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
    if !pr_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::ChangeRequests,
            label: labels.change_requests.clone(),
            items: pr_items.iter().map(|i| (*i).clone()).collect(),
        });
    }

    // Remote branches — sorted by branch name
    remote_items.sort_by(|a, b| a.branch.cmp(&b.branch));
    if !remote_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::RemoteBranches,
            label: "Remote Branches".into(),
            items: remote_items.iter().map(|i| (*i).clone()).collect(),
        });
    }

    // Issues — sorted by id descending
    issue_items.sort_by(|a, b| {
        let a_num = a.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
    if !issue_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::Issues,
            label: labels.issues.clone(),
            items: issue_items.iter().map(|i| (*i).clone()).collect(),
        });
    }

    sections
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core --locked group_work_items_split`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/data.rs crates/flotilla-core/src/data/tests.rs
git commit -m "feat: implement group_work_items_split for structured section output"
```

---

### Task 3: Build `SectionTable<T>` — the generic table widget

**Files:**
- Create: `crates/flotilla-tui/src/widgets/section_table.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/flotilla-tui/src/widgets/section_table.rs` with the type definitions and tests at the bottom. The tests validate selection logic with a simple test type, not `WorkItem`:

```rust
use std::collections::HashMap;

use flotilla_protocol::ProviderData;
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Cell, HighlightSpacing, Row, Table, TableState},
    Frame,
};

use crate::theme::Theme;

/// Contextual dependencies available to column extractors during rendering.
pub struct RenderCtx<'a> {
    pub theme: &'a Theme,
    pub providers: &'a ProviderData,
    pub col_widths: Vec<u16>,
}

/// A column definition for a `SectionTable<T>`.
///
/// Bundles header text, width constraint, and an extractor that produces a
/// ratatui `Cell` from a row value and render context.
pub struct ColumnDef<T> {
    pub header: String,
    pub width: Constraint,
    pub extract: Box<dyn Fn(&T, &RenderCtx) -> Cell<'static>>,
}

/// Trait that section table rows must implement for selection-by-identity
/// preservation across data updates.
pub trait Identifiable {
    type Id: PartialEq + Clone;
    fn id(&self) -> Self::Id;
}

/// A generic table widget that renders rows of type `T` with configurable columns.
///
/// Handles its own selection state (next, prev, select-by-identity) and rendering.
/// Does not render section divider headers — that is the composing widget's job.
pub struct SectionTable<T: Identifiable> {
    pub columns: Vec<ColumnDef<T>>,
    pub items: Vec<T>,
    pub table_state: TableState,
    pub selected_idx: Option<usize>,
    pub header_label: String,
}

impl<T: Identifiable> SectionTable<T> {
    pub fn new(header_label: String, columns: Vec<ColumnDef<T>>) -> Self {
        Self {
            columns,
            items: Vec::new(),
            table_state: TableState::default(),
            selected_idx: None,
            header_label,
        }
    }

    /// Replace items and restore selection by identity.
    pub fn update_items(&mut self, items: Vec<T>) {
        let prev_id = self.selected_idx.and_then(|i| self.items.get(i)).map(|item| item.id());

        self.items = items;

        if self.items.is_empty() {
            self.selected_idx = None;
            self.table_state.select(None);
        } else if let Some(ref prev) = prev_id {
            if let Some(pos) = self.items.iter().position(|item| item.id() == *prev) {
                self.selected_idx = Some(pos);
                self.table_state.select(Some(pos));
            } else {
                self.selected_idx = Some(0);
                self.table_state.select(Some(0));
            }
        } else {
            self.selected_idx = Some(0);
            self.table_state.select(Some(0));
        }
    }

    pub fn select_next(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        match self.selected_idx {
            Some(i) if i + 1 < self.items.len() => {
                self.selected_idx = Some(i + 1);
                self.table_state.select(Some(i + 1));
                true
            }
            Some(_) => false, // at end
            None => {
                self.selected_idx = Some(0);
                self.table_state.select(Some(0));
                true
            }
        }
    }

    pub fn select_prev(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        match self.selected_idx {
            Some(i) if i > 0 => {
                self.selected_idx = Some(i - 1);
                self.table_state.select(Some(i - 1));
                true
            }
            Some(_) => false, // at start
            None => {
                self.selected_idx = Some(0);
                self.table_state.select(Some(0));
                true
            }
        }
    }

    pub fn select_idx(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.selected_idx = Some(idx);
            self.table_state.select(Some(idx));
        }
    }

    pub fn select_first(&mut self) {
        if !self.items.is_empty() {
            self.selected_idx = Some(0);
            self.table_state.select(Some(0));
        }
    }

    pub fn select_last(&mut self) {
        if !self.items.is_empty() {
            let last = self.items.len() - 1;
            self.selected_idx = Some(last);
            self.table_state.select(Some(last));
        }
    }

    pub fn selected_item(&self) -> Option<&T> {
        self.selected_idx.and_then(|i| self.items.get(i))
    }

    pub fn clear_selection(&mut self) {
        self.selected_idx = None;
        self.table_state.select(None);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct TestRow {
        id: u32,
        name: String,
    }

    impl Identifiable for TestRow {
        type Id = u32;
        fn id(&self) -> u32 {
            self.id
        }
    }

    fn test_columns() -> Vec<ColumnDef<TestRow>> {
        vec![ColumnDef {
            header: "Name".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|row: &TestRow, _ctx: &RenderCtx| Cell::from(row.name.clone())),
        }]
    }

    fn rows(ids: &[u32]) -> Vec<TestRow> {
        ids.iter().map(|&id| TestRow { id, name: format!("row-{id}") }).collect()
    }

    #[test]
    fn update_items_preserves_selection_by_identity() {
        let mut table = SectionTable::new("Test".into(), test_columns());
        table.update_items(rows(&[1, 2, 3]));
        table.select_next(); // select row 2 (index 1)
        assert_eq!(table.selected_idx, Some(1));

        // Reorder: row 2 moves to index 2
        table.update_items(rows(&[1, 3, 2]));
        assert_eq!(table.selected_idx, Some(2));
    }

    #[test]
    fn update_items_falls_back_to_first_when_removed() {
        let mut table = SectionTable::new("Test".into(), test_columns());
        table.update_items(rows(&[1, 2, 3]));
        table.select_next(); // select row 2
        table.update_items(rows(&[1, 3])); // row 2 gone
        assert_eq!(table.selected_idx, Some(0));
    }

    #[test]
    fn update_items_clears_on_empty() {
        let mut table = SectionTable::new("Test".into(), test_columns());
        table.update_items(rows(&[1]));
        assert_eq!(table.selected_idx, Some(0));
        table.update_items(vec![]);
        assert_eq!(table.selected_idx, None);
    }

    #[test]
    fn select_next_advances_and_stops_at_end() {
        let mut table = SectionTable::new("Test".into(), test_columns());
        table.update_items(rows(&[1, 2, 3]));

        assert!(table.select_next()); // 0 -> 1
        assert!(table.select_next()); // 1 -> 2
        assert!(!table.select_next()); // at end, returns false
        assert_eq!(table.selected_idx, Some(2));
    }

    #[test]
    fn select_prev_retreats_and_stops_at_start() {
        let mut table = SectionTable::new("Test".into(), test_columns());
        table.update_items(rows(&[1, 2, 3]));
        table.select_next();
        table.select_next(); // at index 2

        assert!(table.select_prev()); // 2 -> 1
        assert!(table.select_prev()); // 1 -> 0
        assert!(!table.select_prev()); // at start, returns false
        assert_eq!(table.selected_idx, Some(0));
    }

    #[test]
    fn select_next_noop_on_empty() {
        let mut table: SectionTable<TestRow> = SectionTable::new("Test".into(), test_columns());
        assert!(!table.select_next());
        assert_eq!(table.selected_idx, None);
    }

    #[test]
    fn select_prev_noop_on_empty() {
        let mut table: SectionTable<TestRow> = SectionTable::new("Test".into(), test_columns());
        assert!(!table.select_prev());
        assert_eq!(table.selected_idx, None);
    }
}
```

- [ ] **Step 2: Add module declaration**

In `crates/flotilla-tui/src/widgets/mod.rs`, add after the `work_item_table` line:

```rust
pub mod section_table;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --locked section_table`
Expected: PASS — the generic selection logic works with `TestRow`

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/widgets/section_table.rs crates/flotilla-tui/src/widgets/mod.rs
git commit -m "feat: add generic SectionTable<T> widget with selection logic"
```

---

### Task 4: Add rendering to `SectionTable<T>`

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/section_table.rs`

- [ ] **Step 1: Add the render method**

Add this method to `impl<T: Identifiable> SectionTable<T>`:

```rust
    /// Render the table rows into the given area.
    ///
    /// The caller is responsible for rendering the section divider header above
    /// this area. This method renders the column header row and item rows.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderCtx, highlight_style: Style) {
        let widths: Vec<Constraint> = self.columns.iter().map(|c| c.width).collect();

        let header_cells: Vec<Cell> = self.columns.iter().map(|c| Cell::from(c.header.clone())).collect();
        let header = Row::new(header_cells).style(Style::default().fg(ctx.theme.muted).bold()).height(1);

        let rows: Vec<Row> = self
            .items
            .iter()
            .map(|item| {
                let cells: Vec<Cell> = self.columns.iter().map(|col| (col.extract)(item, ctx)).collect();
                Row::new(cells)
            })
            .collect();

        let table = Table::new(rows, &widths)
            .header(header)
            .row_highlight_style(highlight_style)
            .highlight_symbol("▸ ")
            .highlight_spacing(HighlightSpacing::Always);

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    /// Hit-test a y-coordinate within the section's rendered area.
    ///
    /// Returns the item index if the coordinate maps to a data row, accounting for
    /// the header row and scroll offset. `area` must be the same `Rect` passed to
    /// `render()`.
    pub fn row_at_y(&self, y: u16, area: Rect) -> Option<usize> {
        if y < area.y || y >= area.y + area.height {
            return None;
        }
        let row_in_widget = (y - area.y) as usize;
        // Row 0 = border top, row 1 = column header
        if row_in_widget < 2 {
            return None;
        }
        let data_row = row_in_widget - 2;
        let idx = data_row + self.table_state.offset();
        if idx < self.items.len() {
            Some(idx)
        } else {
            None
        }
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-tui --locked`
Expected: compiles (render method defined, not yet called)

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-tui/src/widgets/section_table.rs
git commit -m "feat: add rendering and hit-testing to SectionTable"
```

---

### Task 5: Define per-section column sets

**Files:**
- Create: `crates/flotilla-tui/src/widgets/columns.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`

- [ ] **Step 1: Create the columns module**

Create `crates/flotilla-tui/src/widgets/columns.rs`. This module defines `columns_for_section()` which returns the appropriate `Vec<ColumnDef<WorkItem>>` for each `SectionKind`. The extractor closures move the logic currently in `build_item_row()` into per-cell functions.

```rust
use flotilla_core::data::SectionKind;
use flotilla_protocol::{SessionStatus, WorkItem, WorkItemKind};
use ratatui::{
    layout::Constraint,
    style::Style,
    text::Span,
    widgets::Cell,
};

use super::section_table::{ColumnDef, RenderCtx};
use crate::ui_helpers;

/// Build the column definitions for a given section kind.
pub fn columns_for_section(kind: SectionKind) -> Vec<ColumnDef<WorkItem>> {
    match kind {
        SectionKind::Checkouts => checkout_columns(),
        SectionKind::AttachableSets => attachable_set_columns(),
        SectionKind::CloudAgents => cloud_agent_columns(),
        SectionKind::ChangeRequests => change_request_columns(),
        SectionKind::RemoteBranches => remote_branch_columns(),
        SectionKind::Issues => issue_columns(),
    }
}

fn icon_column() -> ColumnDef<WorkItem> {
    ColumnDef {
        header: String::new(),
        width: Constraint::Length(3),
        extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
            let session_status = item
                .session_key
                .as_deref()
                .and_then(|k| ctx.providers.sessions.get(k))
                .map(|s| &s.status);
            let (icon, color) = ui_helpers::work_item_icon(
                &item.kind,
                !item.workspace_refs.is_empty(),
                session_status,
                ctx.theme,
            );
            Cell::from(Span::styled(format!(" {icon}"), Style::default().fg(color)))
        }),
    }
}

fn checkout_columns() -> Vec<ColumnDef<WorkItem>> {
    vec![
        icon_column(),
        ColumnDef {
            header: "Source".into(),
            width: Constraint::Length(10),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item.source.as_deref().unwrap_or("").to_string();
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.source)))
            }),
        },
        ColumnDef {
            header: "Path".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(p) = item.checkout_key() {
                    // Note: repo_root and home_dir would need to be in RenderCtx
                    // for full fidelity. For now, use a simplified display.
                    p.path.display().to_string()
                } else if let Some(ref ses_key) = item.session_key {
                    ses_key.clone()
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.path)))
            }),
        },
        ColumnDef {
            header: "Description".into(),
            width: Constraint::Fill(2),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                Cell::from(Span::styled(item.description.clone(), Style::default().fg(ctx.theme.text)))
            }),
        },
        ColumnDef {
            header: "Branch".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let branch = item.branch.as_deref().unwrap_or("\u{2014}");
                Cell::from(Span::styled(branch.to_string(), Style::default().fg(ctx.theme.branch)))
            }),
        },
        ColumnDef {
            header: "WT".into(),
            width: Constraint::Length(3),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let ind = ui_helpers::checkout_indicator(item.is_main_checkout, item.checkout_key().is_some());
                Cell::from(Span::styled(ind.to_string(), Style::default().fg(ctx.theme.checkout)))
            }),
        },
        ColumnDef {
            header: "WS".into(),
            width: Constraint::Length(3),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let ind = ui_helpers::workspace_indicator(item.workspace_refs.len());
                Cell::from(Span::styled(ind, Style::default().fg(ctx.theme.workspace)))
            }),
        },
        ColumnDef {
            header: "PR".into(),
            width: Constraint::Length(4),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(ref pr_key) = item.change_request_key {
                    if let Some(cr) = ctx.providers.change_requests.get(pr_key.as_str()) {
                        let icon = ui_helpers::change_request_status_icon(&cr.status);
                        format!("#{pr_key}{icon}")
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.change_request)))
            }),
        },
        ColumnDef {
            header: "SS".into(),
            width: Constraint::Length(4),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(ref ses_key) = item.session_key {
                    if let Some(ses) = ctx.providers.sessions.get(ses_key.as_str()) {
                        ui_helpers::session_status_display(&ses.status).to_string()
                    } else {
                        String::new()
                    }
                } else if let Some(agent_key) = item.agent_keys.first() {
                    if let Some(agent) = ctx.providers.agents.get(agent_key.as_str()) {
                        ui_helpers::agent_status_display(&agent.status)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.session)))
            }),
        },
        ColumnDef {
            header: "Issues".into(),
            width: Constraint::Length(6),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item.issue_keys.iter().map(|k| format!("#{k}")).collect::<Vec<_>>().join(",");
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.issue)))
            }),
        },
        ColumnDef {
            header: "Git".into(),
            width: Constraint::Length(5),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(wt_key) = item.checkout_key() {
                    if let Some(co) = ctx.providers.checkouts.get(wt_key) {
                        ui_helpers::git_status_display(co)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.git_status)))
            }),
        },
    ]
}

fn cloud_agent_columns() -> Vec<ColumnDef<WorkItem>> {
    vec![
        icon_column(),
        ColumnDef {
            header: "Source".into(),
            width: Constraint::Length(10),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item.source.as_deref().unwrap_or("").to_string();
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.source)))
            }),
        },
        ColumnDef {
            header: "Key".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item.session_key.as_deref().unwrap_or("").to_string();
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.path)))
            }),
        },
        ColumnDef {
            header: "Description".into(),
            width: Constraint::Fill(2),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                Cell::from(Span::styled(item.description.clone(), Style::default().fg(ctx.theme.text)))
            }),
        },
        ColumnDef {
            header: "Branch".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let branch = item.branch.as_deref().unwrap_or("\u{2014}");
                Cell::from(Span::styled(branch.to_string(), Style::default().fg(ctx.theme.branch)))
            }),
        },
        ColumnDef {
            header: "Status".into(),
            width: Constraint::Length(8),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(ref ses_key) = item.session_key {
                    if let Some(ses) = ctx.providers.sessions.get(ses_key.as_str()) {
                        ui_helpers::session_status_display(&ses.status).to_string()
                    } else {
                        String::new()
                    }
                } else if let Some(agent_key) = item.agent_keys.first() {
                    if let Some(agent) = ctx.providers.agents.get(agent_key.as_str()) {
                        ui_helpers::agent_status_display(&agent.status)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.session)))
            }),
        },
    ]
}

fn change_request_columns() -> Vec<ColumnDef<WorkItem>> {
    vec![
        icon_column(),
        ColumnDef {
            header: "PR#".into(),
            width: Constraint::Length(6),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(ref pr_key) = item.change_request_key {
                    if let Some(cr) = ctx.providers.change_requests.get(pr_key.as_str()) {
                        let icon = ui_helpers::change_request_status_icon(&cr.status);
                        format!("#{pr_key}{icon}")
                    } else {
                        format!("#{pr_key}")
                    }
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.change_request)))
            }),
        },
        ColumnDef {
            header: "Title".into(),
            width: Constraint::Fill(2),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                Cell::from(Span::styled(item.description.clone(), Style::default().fg(ctx.theme.text)))
            }),
        },
        ColumnDef {
            header: "Branch".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let branch = item.branch.as_deref().unwrap_or("\u{2014}");
                Cell::from(Span::styled(branch.to_string(), Style::default().fg(ctx.theme.branch)))
            }),
        },
        ColumnDef {
            header: "State".into(),
            width: Constraint::Length(8),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = if let Some(ref pr_key) = item.change_request_key {
                    if let Some(cr) = ctx.providers.change_requests.get(pr_key.as_str()) {
                        format!("{}", cr.status)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.change_request)))
            }),
        },
        ColumnDef {
            header: "Issues".into(),
            width: Constraint::Length(8),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item.issue_keys.iter().map(|k| format!("#{k}")).collect::<Vec<_>>().join(",");
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.issue)))
            }),
        },
    ]
}

fn issue_columns() -> Vec<ColumnDef<WorkItem>> {
    vec![
        icon_column(),
        ColumnDef {
            header: "ID".into(),
            width: Constraint::Length(6),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item
                    .issue_keys
                    .first()
                    .map(|k| format!("#{k}"))
                    .unwrap_or_default();
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.issue)))
            }),
        },
        ColumnDef {
            header: "Title".into(),
            width: Constraint::Fill(2),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                Cell::from(Span::styled(item.description.clone(), Style::default().fg(ctx.theme.text)))
            }),
        },
        ColumnDef {
            header: "Labels".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                // Labels come from the Issue in providers.issues
                let text = item
                    .issue_keys
                    .first()
                    .and_then(|k| ctx.providers.issues.get(k.as_str()))
                    .map(|issue| issue.labels.join(", "))
                    .unwrap_or_default();
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.muted)))
            }),
        },
        ColumnDef {
            header: "PR".into(),
            width: Constraint::Length(6),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let text = item
                    .change_request_key
                    .as_ref()
                    .map(|k| format!("#{k}"))
                    .unwrap_or_default();
                Cell::from(Span::styled(text, Style::default().fg(ctx.theme.change_request)))
            }),
        },
    ]
}

fn attachable_set_columns() -> Vec<ColumnDef<WorkItem>> {
    vec![
        icon_column(),
        ColumnDef {
            header: "Description".into(),
            width: Constraint::Fill(2),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                Cell::from(Span::styled(item.description.clone(), Style::default().fg(ctx.theme.text)))
            }),
        },
    ]
}

fn remote_branch_columns() -> Vec<ColumnDef<WorkItem>> {
    vec![
        icon_column(),
        ColumnDef {
            header: "Branch".into(),
            width: Constraint::Fill(1),
            extract: Box::new(|item: &WorkItem, ctx: &RenderCtx| {
                let branch = item.branch.as_deref().unwrap_or("\u{2014}");
                Cell::from(Span::styled(branch.to_string(), Style::default().fg(ctx.theme.branch)))
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_for_section_returns_expected_counts() {
        assert_eq!(columns_for_section(SectionKind::Checkouts).len(), 11);
        assert_eq!(columns_for_section(SectionKind::CloudAgents).len(), 6);
        assert_eq!(columns_for_section(SectionKind::ChangeRequests).len(), 6);
        assert_eq!(columns_for_section(SectionKind::Issues).len(), 5);
        assert_eq!(columns_for_section(SectionKind::AttachableSets).len(), 2);
        assert_eq!(columns_for_section(SectionKind::RemoteBranches).len(), 2);
    }

    #[test]
    fn issue_columns_have_expected_headers() {
        let cols = issue_columns();
        let headers: Vec<&str> = cols.iter().map(|c| c.header.as_str()).collect();
        assert_eq!(headers, vec!["", "ID", "Title", "Labels", "PR"]);
    }
}
```

Note: this file references `ui_helpers` functions. You will need to verify that functions like `work_item_icon`, `checkout_indicator`, `workspace_indicator`, `change_request_status_icon`, `session_status_display`, `agent_status_display`, and `git_status_display` are accessible from here. They are in `crates/flotilla-tui/src/ui_helpers.rs` and should be importable via `crate::ui_helpers`.

- [ ] **Step 2: Add module declaration**

In `crates/flotilla-tui/src/widgets/mod.rs`, add:

```rust
pub mod columns;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --locked columns`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/widgets/columns.rs crates/flotilla-tui/src/widgets/mod.rs
git commit -m "feat: define per-section column sets for all section kinds"
```

---

### Task 6: Build `SplitTable` — section composition

**Files:**
- Create: `crates/flotilla-tui/src/widgets/split_table.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`

- [ ] **Step 1: Create the `SplitTable` widget**

Create `crates/flotilla-tui/src/widgets/split_table.rs`:

```rust
use std::collections::{HashMap, HashSet};

use flotilla_core::data::{SectionData, SectionKind};
use flotilla_protocol::{ProviderData, WorkItem, WorkItemIdentity};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Span,
    Frame,
};

use super::{
    columns::columns_for_section,
    section_table::{Identifiable, RenderCtx, SectionTable},
};
use crate::{
    app::{ui_state::PendingAction, TuiModel, UiState},
    theme::Theme,
};

/// Implement `Identifiable` for `WorkItem` so `SectionTable` can preserve selection.
impl Identifiable for WorkItem {
    type Id = WorkItemIdentity;
    fn id(&self) -> WorkItemIdentity {
        self.identity.clone()
    }
}

/// Composes multiple `SectionTable<WorkItem>` instances, one per section kind.
///
/// Handles cross-section navigation, height allocation, rendering of section
/// divider headers, and exposes the same `selected_work_item()` API that
/// `RepoPage` expects.
pub struct SplitTable {
    /// Ordered sections. Only non-empty sections are present.
    sections: Vec<SectionTable<WorkItem>>,
    /// Which section currently has focus (index into `sections`).
    active_section: usize,
    /// Stored from render for mouse hit-testing.
    pub(crate) table_area: Rect,
    /// Per-section rendered areas, for mouse dispatch.
    section_areas: Vec<Rect>,
    /// Gear icon area.
    pub(crate) gear_area: Option<Rect>,
}

impl SplitTable {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
            active_section: 0,
            table_area: Rect::default(),
            section_areas: Vec::new(),
            gear_area: None,
        }
    }

    /// Replace section data. Existing sections update in place (preserving selection).
    /// Sections with no items are dropped. New sections are created with column definitions
    /// from `columns_for_section()`.
    pub fn update_sections(&mut self, section_data: Vec<SectionData>) {
        // Build a lookup of existing sections by kind for reuse
        let mut old_sections: HashMap<SectionKind, SectionTable<WorkItem>> = HashMap::new();
        for section in self.sections.drain(..) {
            // Recover the kind from the section — we store it in a tag
            if let Some(kind) = section_kind_from_label(&section.header_label, &section_data) {
                old_sections.insert(kind, section);
            }
        }

        let mut new_sections = Vec::new();
        for data in section_data {
            if data.items.is_empty() {
                continue;
            }
            let mut section = if let Some(mut existing) = old_sections.remove(&data.kind) {
                existing.header_label = data.label;
                existing.update_items(data.items);
                existing
            } else {
                let columns = columns_for_section(data.kind);
                let mut section = SectionTable::new(data.label, columns);
                section.update_items(data.items);
                section
            };
            new_sections.push(section);
        }

        self.sections = new_sections;
        if self.active_section >= self.sections.len() {
            self.active_section = self.sections.len().saturating_sub(1);
        }
    }

    // ── Navigation ──

    pub fn select_next(&mut self) {
        if self.sections.is_empty() {
            return;
        }
        // Try to advance within the active section
        if !self.sections[self.active_section].select_next() {
            // At end of section — move to next non-empty section
            if self.active_section + 1 < self.sections.len() {
                self.active_section += 1;
                self.sections[self.active_section].select_first();
            }
        }
    }

    pub fn select_prev(&mut self) {
        if self.sections.is_empty() {
            return;
        }
        if !self.sections[self.active_section].select_prev() {
            // At start of section — move to previous non-empty section
            if self.active_section > 0 {
                self.active_section -= 1;
                self.sections[self.active_section].select_last();
            }
        }
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        self.sections.get(self.active_section)?.selected_item()
    }

    pub fn clear_selection(&mut self) {
        if let Some(section) = self.sections.get_mut(self.active_section) {
            section.clear_selection();
        }
    }

    /// Hit-test a mouse position. Returns the selectable index suitable for
    /// `select_by_mouse()`, or `None` if the position is outside data rows.
    pub fn row_at_mouse(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        if x < self.table_area.x || x >= self.table_area.x + self.table_area.width {
            return None;
        }
        for (section_idx, area) in self.section_areas.iter().enumerate() {
            if let Some(item_idx) = self.sections[section_idx].row_at_y(y, *area) {
                return Some((section_idx, item_idx));
            }
        }
        None
    }

    /// Select by section and item index (from `row_at_mouse`).
    pub fn select_by_mouse(&mut self, section_idx: usize, item_idx: usize) {
        if section_idx < self.sections.len() {
            self.active_section = section_idx;
            self.sections[section_idx].select_idx(item_idx);
        }
    }

    /// Access all items across all sections (for multi-select, issue key collection, etc.)
    pub fn all_items(&self) -> impl Iterator<Item = &WorkItem> {
        self.sections.iter().flat_map(|s| s.items.iter())
    }

    /// The currently selected item's identity, if any.
    pub fn selected_identity(&self) -> Option<WorkItemIdentity> {
        self.selected_work_item().map(|item| item.identity.clone())
    }

    // ── Rendering ──

    pub fn render(
        &mut self,
        model: &TuiModel,
        ui: &mut UiState,
        theme: &Theme,
        frame: &mut Frame,
        area: Rect,
        show_providers: bool,
        multi_selected: &HashSet<WorkItemIdentity>,
        pending_actions: &HashMap<WorkItemIdentity, PendingAction>,
    ) {
        self.table_area = area;
        ui.layout.table_area = area;

        if show_providers {
            // Delegate to the provider table rendering (kept from old WorkItemTable)
            // For now, this is a TODO — we'll port the provider rendering in the migration task.
            return;
        }

        let gear_x = area.x + area.width.saturating_sub(5);
        self.gear_area = Some(Rect::new(gear_x, area.y, 3, 1));

        if self.sections.is_empty() {
            return;
        }

        // Allocate height: 1 line per section header + proportional item rows
        let total_items: usize = self.sections.iter().map(|s| s.len()).sum();
        let available_height = area.height as usize;
        // Each section needs: 1 for divider header + 2 for border+column header + item rows
        let header_overhead: usize = self.sections.len(); // 1 divider line per section

        let rm = model.active();
        let render_ctx = RenderCtx {
            theme,
            providers: &rm.providers,
            col_widths: Vec::new(), // individual sections compute their own
        };
        let highlight_style = Style::default().bg(theme.row_highlight).bold();
        let header_style = theme.header_style();

        // Simple height allocation: proportional to item count, minimum 3 rows (header + 1 item)
        let usable = available_height.saturating_sub(header_overhead);
        let mut section_heights: Vec<u16> = Vec::new();
        for section in &self.sections {
            let proportion = if total_items > 0 {
                (section.len() as f64 / total_items as f64 * usable as f64).round() as u16
            } else {
                0
            };
            section_heights.push(proportion.max(3)); // at least border + header + 1 row
        }

        // Lay out sections vertically
        let constraints: Vec<Constraint> = section_heights.iter().map(|&h| Constraint::Length(h + 1)).collect(); // +1 for divider
        let chunks = Layout::vertical(constraints).split(area);

        self.section_areas.clear();
        for (i, section) in self.sections.iter_mut().enumerate() {
            let chunk = chunks[i];
            if chunk.height < 2 {
                self.section_areas.push(Rect::default());
                continue;
            }

            // Render section divider header (1 line)
            let divider_area = Rect::new(chunk.x, chunk.y, chunk.width, 1);
            let label = format!("\u{2500}\u{2500} {} \u{2500}\u{2500}", section.header_label);
            frame.render_widget(Span::styled(label, header_style), divider_area);

            // Render section table below the divider
            let table_area = Rect::new(chunk.x, chunk.y + 1, chunk.width, chunk.height.saturating_sub(1));
            self.section_areas.push(table_area);
            section.render(frame, table_area, &render_ctx, highlight_style);
        }
    }
}

impl Default for SplitTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Recover `SectionKind` from a section's label by matching against the incoming data.
/// Fallback: match on well-known label prefixes.
fn section_kind_from_label(label: &str, section_data: &[SectionData]) -> Option<SectionKind> {
    section_data.iter().find(|d| d.label == label).map(|d| d.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_core::data::SectionData;

    // Use the test builder from the TUI test support
    fn test_work_item(kind: flotilla_protocol::WorkItemKind, id: &str) -> WorkItem {
        WorkItem {
            kind,
            identity: WorkItemIdentity::Issue(id.into()),
            host: "localhost".into(),
            branch: None,
            description: format!("Item {id}"),
            checkout: None,
            change_request_key: None,
            session_key: None,
            issue_keys: if kind == flotilla_protocol::WorkItemKind::Issue { vec![id.into()] } else { vec![] },
            workspace_refs: vec![],
            is_main_checkout: false,
            debug_group: vec![],
            source: None,
            terminal_keys: vec![],
            attachable_set_id: None,
            agent_keys: vec![],
        }
    }

    fn issue_section(ids: &[&str]) -> SectionData {
        SectionData {
            kind: SectionKind::Issues,
            label: "Issues".into(),
            items: ids.iter().map(|id| test_work_item(flotilla_protocol::WorkItemKind::Issue, id)).collect(),
        }
    }

    fn checkout_section(ids: &[&str]) -> SectionData {
        SectionData {
            kind: SectionKind::Checkouts,
            label: "Checkouts".into(),
            items: ids
                .iter()
                .map(|id| {
                    let mut item = test_work_item(flotilla_protocol::WorkItemKind::Checkout, id);
                    item.identity = WorkItemIdentity::Checkout(flotilla_protocol::HostPath::new("localhost", format!("/tmp/{id}")));
                    item
                })
                .collect(),
        }
    }

    #[test]
    fn cross_section_navigation_next() {
        let mut table = SplitTable::new();
        table.update_sections(vec![checkout_section(&["a", "b"]), issue_section(&["1", "2"])]);

        // Start at first item of first section
        assert_eq!(table.active_section, 0);
        assert_eq!(table.sections[0].selected_idx, Some(0));

        table.select_next(); // checkout b
        assert_eq!(table.active_section, 0);
        assert_eq!(table.sections[0].selected_idx, Some(1));

        table.select_next(); // cross to issues, item 1
        assert_eq!(table.active_section, 1);
        assert_eq!(table.sections[1].selected_idx, Some(0));

        table.select_next(); // issue 2
        assert_eq!(table.active_section, 1);
        assert_eq!(table.sections[1].selected_idx, Some(1));

        table.select_next(); // at end, stays put
        assert_eq!(table.active_section, 1);
        assert_eq!(table.sections[1].selected_idx, Some(1));
    }

    #[test]
    fn cross_section_navigation_prev() {
        let mut table = SplitTable::new();
        table.update_sections(vec![checkout_section(&["a"]), issue_section(&["1", "2"])]);

        // Navigate to issues section
        table.select_next(); // cross to issues
        table.select_next(); // issue 2
        assert_eq!(table.active_section, 1);

        table.select_prev(); // issue 1
        assert_eq!(table.sections[1].selected_idx, Some(0));

        table.select_prev(); // cross back to checkouts
        assert_eq!(table.active_section, 0);
        assert_eq!(table.sections[0].selected_idx, Some(0));

        table.select_prev(); // at start, stays put
        assert_eq!(table.active_section, 0);
    }

    #[test]
    fn update_sections_preserves_selection() {
        let mut table = SplitTable::new();
        table.update_sections(vec![issue_section(&["1", "2", "3"])]);
        table.select_next(); // select issue 2

        // Reorder issues
        table.update_sections(vec![issue_section(&["1", "3", "2"])]);
        // Issue "2" should still be selected, now at index 2
        assert_eq!(table.sections[0].selected_idx, Some(2));
    }

    #[test]
    fn empty_sections_omitted() {
        let mut table = SplitTable::new();
        table.update_sections(vec![
            SectionData { kind: SectionKind::Checkouts, label: "Checkouts".into(), items: vec![] },
            issue_section(&["1"]),
        ]);
        assert_eq!(table.sections.len(), 1);
        assert_eq!(table.sections[0].header_label, "Issues");
    }
}
```

- [ ] **Step 2: Add module declaration**

In `crates/flotilla-tui/src/widgets/mod.rs`, add:

```rust
pub mod split_table;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --locked split_table`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/widgets/split_table.rs crates/flotilla-tui/src/widgets/mod.rs
git commit -m "feat: add SplitTable composing per-section SectionTable instances"
```

---

### Task 7: Wire `SplitTable` into `RepoPage`

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/repo_page.rs`

This is the integration task — replace `WorkItemTable` with `SplitTable` in `RepoPage`.

- [ ] **Step 1: Update imports and struct definition**

In `repo_page.rs`, replace the `WorkItemTable` import and field:

Change the import from:
```rust
use super::{
    preview_panel::PreviewPanel, work_item_table::WorkItemTable, AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
```
to:
```rust
use super::{
    preview_panel::PreviewPanel, split_table::SplitTable, AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
```

In the `RepoPage` struct, change:
```rust
pub table: WorkItemTable,
```
to:
```rust
pub table: SplitTable,
```

In `RepoPage::new()`, change:
```rust
table: WorkItemTable::new(),
```
to:
```rust
table: SplitTable::new(),
```

- [ ] **Step 2: Update `rebuild_table()`**

Replace the body of `rebuild_table()`:

```rust
fn rebuild_table(&mut self, data: &RepoData) {
    let section_labels = SectionLabels {
        checkouts: data.labels.checkouts.section.clone(),
        change_requests: data.labels.change_requests.section.clone(),
        issues: data.labels.issues.section.clone(),
        sessions: data.labels.cloud_agents.section.clone(),
    };
    let sections = flotilla_core::data::group_work_items_split(
        &data.work_items,
        &data.providers,
        &section_labels,
        &data.path,
    );
    // TODO: filter archived sessions — needs to move into group_work_items_split
    // or be applied per-section. For now, pass through unfiltered.
    self.table.update_sections(sections);

    // Prune stale multi_selected and pending_actions
    let current_identities: HashSet<WorkItemIdentity> =
        self.table.all_items().map(|item| item.identity.clone()).collect();
    self.multi_selected.retain(|id| current_identities.contains(id));
    self.pending_actions.retain(|id, _| current_identities.contains(id));
}
```

Update the import at the top of the file — remove `GroupEntry` and add `group_work_items_split` usage (the import is `flotilla_core::data::SectionLabels` which is already imported; `group_work_items_split` is called via fully-qualified path).

- [ ] **Step 3: Update action handlers**

In `handle_action()`, the `SelectNext` / `SelectPrev` calls already delegate to `self.table.select_next()` / `self.table.select_prev()` — these method names match `SplitTable`.

Update `toggle_multi_select()` — it currently accesses `grouped_items` internals. Replace with:

```rust
fn toggle_multi_select(&mut self) {
    if let Some(item) = self.table.selected_work_item() {
        let identity = item.identity.clone();
        if !self.multi_selected.remove(&identity) {
            self.multi_selected.insert(identity);
        }
    }
}
```

Update `select_all()`:

```rust
pub fn select_all(&mut self) {
    for item in self.table.all_items() {
        self.multi_selected.insert(item.identity.clone());
    }
}
```

- [ ] **Step 4: Update mouse handling**

In `handle_mouse()`, replace `row_at_mouse_self` calls with `row_at_mouse` + `select_by_mouse`:

Where the code does:
```rust
if let Some(si) = self.table.row_at_mouse_self(x, y) {
    self.table.select_row_self(si);
```

Replace with:
```rust
if let Some((section_idx, item_idx)) = self.table.row_at_mouse(x, y) {
    self.table.select_by_mouse(section_idx, item_idx);
```

The double-click detection needs updating too — it currently stores `last_selectable_idx: Option<usize>`. Change it to store `Option<(usize, usize)>` for `(section_idx, item_idx)`.

- [ ] **Step 5: Update `render_table()`**

The `render_table()` method delegates to the table's render. Update:

```rust
fn render_table(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
    self.table.table_area = area;
    self.table.render(
        ctx.model,
        ctx.ui,
        ctx.theme,
        frame,
        area,
        self.show_providers,
        &self.multi_selected,
        &self.pending_actions,
    );
}
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo build -p flotilla-tui --locked`
Expected: may have compilation errors in other files that reference `page.table.grouped_items` — those are addressed in the next task.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-tui/src/widgets/repo_page.rs
git commit -m "feat: wire SplitTable into RepoPage, replacing WorkItemTable"
```

---

### Task 8: Update remaining consumers of old table API

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Modify: `crates/flotilla-tui/src/app/navigation/tests.rs`
- Modify: `crates/flotilla-tui/tests/support/high_fidelity.rs`

These files reference `GroupEntry`, `GroupedWorkItems`, or `page.table.grouped_items` and need updating to use the `SplitTable` API.

- [ ] **Step 1: Update `key_handlers.rs`**

In `action_enter_multi_select()` (around line 227), replace the `GroupEntry` iteration:

```rust
// Old:
for entry in &page.table.grouped_items.table_entries {
    if let GroupEntry::Item(item) = entry {
        if multi_selected.contains(&item.identity) {
            all_issue_keys.extend(item.issue_keys.iter().cloned());
        }
    }
}
```

With:
```rust
// New:
for item in page.table.all_items() {
    if multi_selected.contains(&item.identity) {
        all_issue_keys.extend(item.issue_keys.iter().cloned());
    }
}
```

Remove the `use flotilla_core::data::GroupEntry;` import.

- [ ] **Step 2: Update `app/mod.rs`**

Around line 671-676, replace the `GroupEntry` pattern match:

```rust
// Old:
if let Some(flotilla_core::data::GroupEntry::Item(item)) =
    page.table.grouped_items.table_entries.get(table_idx)
```

With:
```rust
// New — use the SplitTable API:
if let Some(item) = page.table.selected_work_item()
```

Review the surrounding code to ensure the logic still holds — the old code was extracting the item at a specific index for the toggle-multi-select `AppAction`. The new code should use `selected_work_item()` directly.

- [ ] **Step 3: Update `test_support.rs`**

Replace the `grouped_items()` and `set_active_table_view()` helpers:

```rust
// Old:
pub(crate) fn grouped_items(items: Vec<WorkItem>) -> GroupedWorkItems { ... }
pub(crate) fn set_active_table_view(app: &mut App, table_view: GroupedWorkItems) { ... }

// New:
pub(crate) fn set_active_table_sections(app: &mut App, sections: Vec<flotilla_core::data::SectionData>) {
    let repo_key = app.model.repo_order[app.model.active_repo].clone();
    if let Some(page) = app.screen.repo_pages.get_mut(&repo_key) {
        page.table.update_sections(sections);
    }
}
```

Update any test helper that builds `GroupedWorkItems` to build `Vec<SectionData>` instead.

- [ ] **Step 4: Update `navigation/tests.rs`**

These tests construct `GroupedWorkItems` with `GroupEntry::Header` and `GroupEntry::Item`. Rewrite them to use `SectionData` and `SplitTable` operations. The test at line ~328 that constructs entries with a header gap needs to become a two-section test where navigation crosses section boundaries.

- [ ] **Step 5: Update `high_fidelity.rs`**

Replace the `GroupEntry` references with equivalent `SplitTable` API calls.

- [ ] **Step 6: Verify everything compiles and tests pass**

Run: `cargo build --workspace --locked && cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/test_support.rs crates/flotilla-tui/src/app/navigation/tests.rs crates/flotilla-tui/tests/support/high_fidelity.rs
git commit -m "refactor: update all consumers to use SplitTable API"
```

---

### Task 9: Add archived session filtering to `group_work_items_split`

**Files:**
- Modify: `crates/flotilla-core/src/data.rs`
- Test: `crates/flotilla-core/src/data/tests.rs`

The old `GroupedWorkItems::filter_archived_sessions()` filtered the flat list. We need equivalent functionality for the structured output.

- [ ] **Step 1: Write the failing test**

In `crates/flotilla-core/src/data/tests.rs`:

```rust
#[test]
fn filter_archived_sessions_from_section_data() {
    use crate::data::{filter_archived_sections, SectionKind};

    let active = test_session_work_item("s1");
    let archived = test_session_work_item("s2");

    let sections = vec![SectionData {
        kind: SectionKind::CloudAgents,
        label: "Cloud Agents".into(),
        items: vec![active, archived],
    }];

    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Running));
    providers.sessions.insert("s2".into(), test_cloud_agent_session(SessionStatus::Archived));

    let filtered = filter_archived_sections(sections, &providers);

    // Cloud Agents section should only have the active session
    let agents = filtered.iter().find(|s| s.kind == SectionKind::CloudAgents).expect("should have agents section");
    assert_eq!(agents.items.len(), 1);
    assert_eq!(agents.items[0].session_key.as_deref(), Some("s1"));
}

#[test]
fn filter_archived_removes_empty_section() {
    use crate::data::{filter_archived_sections, SectionKind};

    let archived = test_session_work_item("s1");
    let sections = vec![SectionData {
        kind: SectionKind::CloudAgents,
        label: "Cloud Agents".into(),
        items: vec![archived],
    }];

    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Archived));

    let filtered = filter_archived_sections(sections, &providers);
    assert!(filtered.iter().all(|s| s.kind != SectionKind::CloudAgents));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p flotilla-core --locked filter_archived_sections`
Expected: compilation error — `filter_archived_sections` not defined

- [ ] **Step 3: Implement `filter_archived_sections()`**

In `crates/flotilla-core/src/data.rs`:

```rust
/// Filter archived/expired sessions from structured section data.
/// Removes sessions with archived or expired status. Drops sections that become empty.
/// Agent items are never filtered.
pub fn filter_archived_sections(sections: Vec<SectionData>, providers: &ProviderData) -> Vec<SectionData> {
    use flotilla_protocol::SessionStatus;

    sections
        .into_iter()
        .filter_map(|mut section| {
            if section.kind == SectionKind::CloudAgents {
                section.items.retain(|item| {
                    if item.kind == WorkItemKind::Session {
                        let is_archived = item
                            .session_key
                            .as_deref()
                            .and_then(|k| providers.sessions.get(k))
                            .is_some_and(|s| matches!(s.status, SessionStatus::Archived | SessionStatus::Expired));
                        !is_archived
                    } else {
                        true // keep agents
                    }
                });
            }
            if section.items.is_empty() { None } else { Some(section) }
        })
        .collect()
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked filter_archived_sections`
Expected: PASS

- [ ] **Step 5: Wire into `RepoPage::rebuild_table`**

In `crates/flotilla-tui/src/widgets/repo_page.rs`, update `rebuild_table()` to call the filter:

```rust
fn rebuild_table(&mut self, data: &RepoData) {
    let section_labels = SectionLabels { ... };
    let sections = flotilla_core::data::group_work_items_split(
        &data.work_items, &data.providers, &section_labels, &data.path,
    );
    let sections = if self.show_archived {
        sections
    } else {
        flotilla_core::data::filter_archived_sections(sections, &data.providers)
    };
    self.table.update_sections(sections);
    // ... prune stale selections ...
}
```

- [ ] **Step 6: Verify**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/data.rs crates/flotilla-core/src/data/tests.rs crates/flotilla-tui/src/widgets/repo_page.rs
git commit -m "feat: add archived session filtering for structured section data"
```

---

### Task 10: Port provider table rendering into `SplitTable`

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/split_table.rs`

The old `WorkItemTable` had `render_providers()` for the gear-icon provider status view. Port it to `SplitTable`.

- [ ] **Step 1: Copy the provider rendering functions**

Move `render_providers()`, `provider_status_badge()`, `provider_row()`, `provider_empty_row()`, `provider_table_header()`, and `provider_table_widths()` from `work_item_table.rs` into `split_table.rs`. Update imports as needed (these use `PROVIDER_CATEGORIES` from `super`).

- [ ] **Step 2: Wire into `SplitTable::render()`**

In the `render()` method, replace the `if show_providers { return; }` stub with a call to the ported `render_providers()`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-tui --locked`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/widgets/split_table.rs
git commit -m "feat: port provider table rendering into SplitTable"
```

---

### Task 11: Remove old `WorkItemTable` and dead types

**Files:**
- Delete: `crates/flotilla-tui/src/widgets/work_item_table.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-core/src/data.rs`

- [ ] **Step 1: Remove the module declaration**

In `crates/flotilla-tui/src/widgets/mod.rs`, remove:
```rust
pub mod work_item_table;
```

- [ ] **Step 2: Delete the file**

```bash
rm crates/flotilla-tui/src/widgets/work_item_table.rs
```

- [ ] **Step 3: Remove dead types from `data.rs`**

If `GroupedWorkItems`, `GroupEntry`, and `SectionHeader` are no longer referenced anywhere, remove them from `crates/flotilla-core/src/data.rs`. Also remove `group_work_items()` if it has no remaining callers. Check with:

```bash
cargo build --workspace --locked 2>&1 | head -50
```

Fix any remaining references. If `group_work_items()` is still used elsewhere (e.g. in `data/tests.rs` for the old-style tests), either remove those tests or migrate them to use `group_work_items_split()`.

- [ ] **Step 4: Run full CI checks**

Run:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: remove WorkItemTable and legacy GroupedWorkItems types"
```

---

### Task 12: Final verification and cleanup

**Files:** All modified files

- [ ] **Step 1: Run full CI gate**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: all pass with no warnings

- [ ] **Step 2: Format if needed**

```bash
cargo +nightly-2026-03-12 fmt
```

- [ ] **Step 3: Run snapshot tests and inspect any changes**

If any snapshot tests fail, inspect the diff. The rendered output will have changed since each section now has its own column layout. If the changes are expected consequences of the split-view design, accept them with justification. If unexpected, investigate.

- [ ] **Step 4: Commit any formatting or snapshot updates**

```bash
git add -A
git commit -m "chore: formatting and snapshot updates for split-view tables"
```
