use flotilla_protocol::ProviderData;
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::Span,
    widgets::{Cell, HighlightSpacing, Row, Table, TableState},
    Frame,
};

use crate::theme::Theme;

// ---------------------------------------------------------------------------
// RenderCtx
// ---------------------------------------------------------------------------

/// Contextual dependencies available to column extractors during rendering.
pub struct RenderCtx<'a> {
    pub theme: &'a Theme,
    pub providers: &'a ProviderData,
    pub col_widths: Vec<u16>,
}

// ---------------------------------------------------------------------------
// ColumnDef
// ---------------------------------------------------------------------------

/// The extractor function type for column definitions.
pub type ExtractFn<T> = dyn Fn(&T, &RenderCtx) -> Cell<'static>;

/// A column definition for a `SectionTable<T>`.
pub struct ColumnDef<T> {
    pub header: String,
    pub width: Constraint,
    pub extract: Box<ExtractFn<T>>,
}

// ---------------------------------------------------------------------------
// Identifiable
// ---------------------------------------------------------------------------

/// Trait that section table rows must implement for selection-by-identity
/// preservation across data updates.
pub trait Identifiable {
    type Id: PartialEq + Clone;
    fn id(&self) -> Self::Id;
}

// ---------------------------------------------------------------------------
// SectionTable
// ---------------------------------------------------------------------------

/// A generic table widget that renders rows of type `T` with configurable
/// columns. Handles selection state (next, prev, select-by-identity) but
/// delegates rendering to the caller.
pub struct SectionTable<T: Identifiable> {
    pub columns: Vec<ColumnDef<T>>,
    pub items: Vec<T>,
    pub table_state: TableState,
    pub selected_idx: Option<usize>,
    pub header_label: String,
}

impl<T: Identifiable> SectionTable<T> {
    pub fn new(header_label: String, columns: Vec<ColumnDef<T>>) -> Self {
        Self { columns, items: Vec::new(), table_state: TableState::default(), selected_idx: None, header_label }
    }

    /// Replace items, restoring selection by identity.
    ///
    /// On the first call (no prior selection), auto-selects the first item.
    /// On subsequent calls, tries to find the previously-selected item by
    /// identity. If found, selects it at its new position. If not found,
    /// falls back to the first item. If items are empty, clears selection.
    pub fn update_items(&mut self, items: Vec<T>) {
        let prev_id = self.selected_idx.and_then(|idx| self.items.get(idx)).map(|item| item.id());

        self.items = items;

        if self.items.is_empty() {
            self.selected_idx = None;
            self.table_state.select(None);
        } else if let Some(ref prev) = prev_id {
            if let Some(new_idx) = self.items.iter().position(|item| item.id() == *prev) {
                self.selected_idx = Some(new_idx);
                self.table_state.select(Some(new_idx));
            } else {
                self.selected_idx = Some(0);
                self.table_state.select(Some(0));
            }
        } else {
            // First call — auto-select first item
            self.selected_idx = Some(0);
            self.table_state.select(Some(0));
        }
    }

    /// Advance selection by one row. Returns `false` if already at the end
    /// (does not wrap). The composing `SplitTable` uses this return value to
    /// know when to cross section boundaries.
    pub fn select_next(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        match self.selected_idx {
            Some(idx) if idx + 1 < self.items.len() => {
                let next = idx + 1;
                self.selected_idx = Some(next);
                self.table_state.select(Some(next));
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

    /// Retreat selection by one row. Returns `false` if already at the start
    /// (does not wrap).
    pub fn select_prev(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        match self.selected_idx {
            Some(idx) if idx > 0 => {
                let prev = idx - 1;
                self.selected_idx = Some(prev);
                self.table_state.select(Some(prev));
                true
            }
            Some(_) => false, // at start
            None => {
                let last = self.items.len() - 1;
                self.selected_idx = Some(last);
                self.table_state.select(Some(last));
                true
            }
        }
    }

    /// Select by item index. No-op if out of bounds.
    pub fn select_idx(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.selected_idx = Some(idx);
            self.table_state.select(Some(idx));
        }
    }

    /// Jump to the first item.
    pub fn select_first(&mut self) {
        if !self.items.is_empty() {
            self.selected_idx = Some(0);
            self.table_state.select(Some(0));
        }
    }

    /// Jump to the last item.
    pub fn select_last(&mut self) {
        if !self.items.is_empty() {
            let last = self.items.len() - 1;
            self.selected_idx = Some(last);
            self.table_state.select(Some(last));
        }
    }

    /// The currently selected item, if any.
    pub fn selected_item(&self) -> Option<&T> {
        self.selected_idx.and_then(|idx| self.items.get(idx))
    }

    /// Clear the current selection.
    pub fn clear_selection(&mut self) {
        self.selected_idx = None;
        self.table_state.select(None);
    }

    /// Whether the table has no items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The number of items in the table.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Render the table into `area`.
    ///
    /// Builds a header row from the column definitions, then one data row per
    /// item using each column's extractor. No block/border is added — the
    /// composing widget owns the outer container.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderCtx, highlight_style: Style) {
        let header = Row::new(self.columns.iter().map(|c| {
            Cell::from(Span::raw(c.header.clone())).style(Style::default().fg(ctx.theme.muted).bold())
        }))
        .height(1);

        let rows: Vec<Row> = self.items.iter().map(|item| Row::new(self.columns.iter().map(|c| (c.extract)(item, ctx)))).collect();

        let widths: Vec<Constraint> = self.columns.iter().map(|c| c.width).collect();

        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(highlight_style)
            .highlight_symbol("▸ ")
            .highlight_spacing(HighlightSpacing::Always);

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    /// Hit-test a y-coordinate against the rendered area.
    ///
    /// Row layout (no block/border on the table itself, but the caller places
    /// the table inside a bordered container, so the table area already
    /// excludes the border):
    ///
    /// - Row 0 (area.y + 0): column header
    /// - Rows 1.. (area.y + 1..): data rows, offset by scroll
    ///
    /// Returns the item index into `self.items`, or `None` if `y` is outside
    /// the area or maps to the header row.
    pub fn row_at_y(&self, y: u16, area: Rect) -> Option<usize> {
        if y < area.y || y >= area.y + area.height {
            return None;
        }
        let relative = (y - area.y) as usize;
        if relative == 0 {
            // Header row
            return None;
        }
        let data_row = relative - 1;
        let idx = self.table_state.offset() + data_row;
        if idx < self.items.len() { Some(idx) } else { None }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Test row type ------------------------------------------------------

    #[derive(Debug, Clone)]
    #[allow(dead_code)] // `name` exercised only for realistic row data
    struct TestRow {
        id: u32,
        name: String,
    }

    impl TestRow {
        fn new(id: u32, name: &str) -> Self {
            Self { id, name: name.to_string() }
        }
    }

    impl Identifiable for TestRow {
        type Id = u32;
        fn id(&self) -> u32 {
            self.id
        }
    }

    fn make_table() -> SectionTable<TestRow> {
        SectionTable::new("Test".to_string(), Vec::new())
    }

    fn rows(ids: &[(u32, &str)]) -> Vec<TestRow> {
        ids.iter().map(|(id, name)| TestRow::new(*id, name)).collect()
    }

    // -- Tests --------------------------------------------------------------

    #[test]
    fn update_items_auto_selects_first_on_initial_call() {
        let mut table = make_table();
        assert_eq!(table.selected_idx, None);

        table.update_items(rows(&[(1, "alpha"), (2, "bravo")]));

        assert_eq!(table.selected_idx, Some(0));
        assert_eq!(table.table_state.selected(), Some(0));
        assert_eq!(table.selected_item().expect("should have selection").id, 1);
    }

    #[test]
    fn update_items_preserves_selection_by_identity() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "alpha"), (2, "bravo"), (3, "charlie")]));

        // Select bravo (id=2)
        table.select_next();
        assert_eq!(table.selected_idx, Some(1));
        assert_eq!(table.selected_item().expect("selected").id, 2);

        // Reorder: bravo moves to position 2
        table.update_items(rows(&[(1, "alpha"), (3, "charlie"), (2, "bravo")]));

        assert_eq!(table.selected_idx, Some(2));
        assert_eq!(table.table_state.selected(), Some(2));
        assert_eq!(table.selected_item().expect("selected").id, 2);
    }

    #[test]
    fn update_items_falls_back_to_first_when_removed() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "alpha"), (2, "bravo")]));

        // Select bravo
        table.select_next();
        assert_eq!(table.selected_item().expect("selected").id, 2);

        // Update without bravo
        table.update_items(rows(&[(1, "alpha"), (3, "charlie")]));

        // Falls back to first
        assert_eq!(table.selected_idx, Some(0));
        assert_eq!(table.table_state.selected(), Some(0));
        assert_eq!(table.selected_item().expect("selected").id, 1);
    }

    #[test]
    fn update_items_clears_on_empty() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "alpha")]));
        assert_eq!(table.selected_idx, Some(0));

        table.update_items(Vec::new());

        assert_eq!(table.selected_idx, None);
        assert_eq!(table.table_state.selected(), None);
        assert!(table.selected_item().is_none());
    }

    #[test]
    fn select_next_advances_and_stops_at_end() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "a"), (2, "b"), (3, "c")]));

        // Starts at 0
        assert_eq!(table.selected_idx, Some(0));

        assert!(table.select_next()); // 0 -> 1
        assert_eq!(table.selected_idx, Some(1));

        assert!(table.select_next()); // 1 -> 2
        assert_eq!(table.selected_idx, Some(2));

        // At end — returns false, stays put
        assert!(!table.select_next());
        assert_eq!(table.selected_idx, Some(2));

        // Still false on repeated attempt
        assert!(!table.select_next());
        assert_eq!(table.selected_idx, Some(2));
    }

    #[test]
    fn select_prev_retreats_and_stops_at_start() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "a"), (2, "b"), (3, "c")]));

        // Move to last
        table.select_last();
        assert_eq!(table.selected_idx, Some(2));

        assert!(table.select_prev()); // 2 -> 1
        assert_eq!(table.selected_idx, Some(1));

        assert!(table.select_prev()); // 1 -> 0
        assert_eq!(table.selected_idx, Some(0));

        // At start — returns false, stays put
        assert!(!table.select_prev());
        assert_eq!(table.selected_idx, Some(0));

        // Still false on repeated attempt
        assert!(!table.select_prev());
        assert_eq!(table.selected_idx, Some(0));
    }

    #[test]
    fn select_next_noop_on_empty() {
        let mut table = make_table();
        assert!(!table.select_next());
        assert_eq!(table.selected_idx, None);
    }

    #[test]
    fn select_prev_noop_on_empty() {
        let mut table = make_table();
        assert!(!table.select_prev());
        assert_eq!(table.selected_idx, None);
    }

    // -- Additional coverage ------------------------------------------------

    #[test]
    fn select_idx_within_bounds() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "a"), (2, "b"), (3, "c")]));

        table.select_idx(2);
        assert_eq!(table.selected_idx, Some(2));
        assert_eq!(table.table_state.selected(), Some(2));
    }

    #[test]
    fn select_idx_out_of_bounds_is_noop() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "a")]));

        table.select_idx(5);
        // Should still be at initial selection (0)
        assert_eq!(table.selected_idx, Some(0));
    }

    #[test]
    fn select_first_and_last() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "a"), (2, "b"), (3, "c")]));

        table.select_last();
        assert_eq!(table.selected_idx, Some(2));
        assert_eq!(table.selected_item().expect("selected").id, 3);

        table.select_first();
        assert_eq!(table.selected_idx, Some(0));
        assert_eq!(table.selected_item().expect("selected").id, 1);
    }

    #[test]
    fn select_first_noop_on_empty() {
        let mut table = make_table();
        table.select_first();
        assert_eq!(table.selected_idx, None);
    }

    #[test]
    fn select_last_noop_on_empty() {
        let mut table = make_table();
        table.select_last();
        assert_eq!(table.selected_idx, None);
    }

    #[test]
    fn clear_selection() {
        let mut table = make_table();
        table.update_items(rows(&[(1, "a"), (2, "b")]));
        assert_eq!(table.selected_idx, Some(0));

        table.clear_selection();
        assert_eq!(table.selected_idx, None);
        assert_eq!(table.table_state.selected(), None);
        assert!(table.selected_item().is_none());
    }

    #[test]
    fn is_empty_and_len() {
        let mut table = make_table();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);

        table.update_items(rows(&[(1, "a"), (2, "b")]));
        assert!(!table.is_empty());
        assert_eq!(table.len(), 2);
    }
}
