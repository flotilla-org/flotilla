use std::collections::{HashMap, HashSet};

use flotilla_core::data::{SectionData, SectionKind};
use flotilla_protocol::{HostName, WorkItem, WorkItemIdentity};
use ratatui::{
    layout::{Constraint, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Row, Table},
    Frame,
};

use super::{
    columns::{columns_for_section, issue_columns_native},
    section_table::{ColumnDef, Identifiable, IssueRow, RenderCtx, SectionTable},
    PROVIDER_CATEGORIES,
};
use crate::{
    app::{
        ui_state::{PendingAction, PendingStatus},
        ProviderStatus, TuiModel, UiState,
    },
    theme::Theme,
    ui_helpers,
};

// ---------------------------------------------------------------------------
// Identifiable impl for WorkItem
// ---------------------------------------------------------------------------

/// Implement `Identifiable` for `WorkItem` so `SectionTable` can preserve selection.
impl Identifiable for WorkItem {
    type Id = WorkItemIdentity;
    fn id(&self) -> WorkItemIdentity {
        self.identity.clone()
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const HIGHLIGHT_SYMBOL: &str = "\u{25b8} ";
const HIGHLIGHT_WIDTH: u16 = 2;
const COL_SPACING: u16 = 1;

// ---------------------------------------------------------------------------
// AnySection — type-erased section for heterogeneous SplitTable
// ---------------------------------------------------------------------------

/// A section that can contain either `WorkItem` rows or native `IssueRow` rows.
///
/// This enum lets `SplitTable` hold both correlation-derived `WorkItem` sections
/// and query-driven `IssueRow` sections in a single flat list, with uniform
/// navigation, rendering, and selection.
pub enum AnySection {
    WorkItems(SectionTable<WorkItem>),
    Issues(SectionTable<IssueRow>),
}

impl AnySection {
    fn row_count(&self) -> usize {
        match self {
            AnySection::WorkItems(t) => t.items.len(),
            AnySection::Issues(t) => t.items.len(),
        }
    }

    fn select_next(&mut self) -> bool {
        match self {
            AnySection::WorkItems(t) => t.select_next(),
            AnySection::Issues(t) => t.select_next(),
        }
    }

    fn select_prev(&mut self) -> bool {
        match self {
            AnySection::WorkItems(t) => t.select_prev(),
            AnySection::Issues(t) => t.select_prev(),
        }
    }

    fn select_first(&mut self) {
        match self {
            AnySection::WorkItems(t) => t.select_first(),
            AnySection::Issues(t) => t.select_first(),
        }
    }

    fn select_last(&mut self) {
        match self {
            AnySection::WorkItems(t) => t.select_last(),
            AnySection::Issues(t) => t.select_last(),
        }
    }

    fn select_idx(&mut self, idx: usize) {
        match self {
            AnySection::WorkItems(t) => t.select_idx(idx),
            AnySection::Issues(t) => t.select_idx(idx),
        }
    }

    fn clear_selection(&mut self) {
        match self {
            AnySection::WorkItems(t) => t.clear_selection(),
            AnySection::Issues(t) => t.clear_selection(),
        }
    }

    fn selected_idx(&self) -> Option<usize> {
        match self {
            AnySection::WorkItems(t) => t.selected_idx,
            AnySection::Issues(t) => t.selected_idx,
        }
    }

    fn header_label(&self) -> &str {
        match self {
            AnySection::WorkItems(t) => &t.header_label,
            AnySection::Issues(t) => &t.header_label,
        }
    }

    /// The selected row, returning a `SelectedRow` enum.
    fn selected_row(&self) -> Option<SelectedRow<'_>> {
        match self {
            AnySection::WorkItems(t) => t.selected_item().map(SelectedRow::WorkItem),
            AnySection::Issues(t) => t.selected_item().map(SelectedRow::Issue),
        }
    }
}

// ---------------------------------------------------------------------------
// SelectedRow — typed return from heterogeneous selection
// ---------------------------------------------------------------------------

/// The currently selected row, which can be either a `WorkItem` or an `IssueRow`.
///
/// Callers match on this to handle both types. `IssueRow` carries native `Issue`
/// data (labels, provider name) that `WorkItem` cannot represent.
#[derive(Debug, Clone)]
pub enum SelectedRow<'a> {
    WorkItem(&'a WorkItem),
    Issue(&'a IssueRow),
}

impl<'a> SelectedRow<'a> {
    /// The description/title of the selected row.
    pub fn description(&self) -> &str {
        match self {
            SelectedRow::WorkItem(item) => &item.description,
            SelectedRow::Issue(row) => &row.issue.title,
        }
    }

    /// Issue keys for the selected row.
    pub fn issue_keys(&self) -> Vec<String> {
        match self {
            SelectedRow::WorkItem(item) => item.issue_keys.clone(),
            SelectedRow::Issue(row) => vec![row.id.clone()],
        }
    }

    /// Whether this row is a WorkItem (for callers that need the full type).
    pub fn as_work_item(&self) -> Option<&'a WorkItem> {
        match self {
            SelectedRow::WorkItem(item) => Some(item),
            SelectedRow::Issue(_) => None,
        }
    }

    /// Whether this row is an IssueRow.
    pub fn as_issue_row(&self) -> Option<&'a IssueRow> {
        match self {
            SelectedRow::WorkItem(_) => None,
            SelectedRow::Issue(row) => Some(row),
        }
    }
}

// ---------------------------------------------------------------------------
// SplitTable
// ---------------------------------------------------------------------------

/// Composes multiple sections — `SectionTable<WorkItem>` for correlation-derived
/// sections and `SectionTable<IssueRow>` for query-driven issues — into a single
/// scrollable surface with one flat cursor.
///
/// Virtual row layout per section:
///   1 divider row + 1 column header row + N data rows
///
/// Only data rows are selectable.
pub struct SplitTable {
    /// Ordered sections with their kind. Only non-empty sections present.
    sections: Vec<(SectionKind, AnySection)>,
    /// Which section currently has focus (index into sections).
    active_section: usize,
    /// Scroll offset — the first visible virtual row.
    scroll_offset: usize,
    /// Stored from render for mouse hit-testing.
    pub(crate) table_area: Rect,
    /// Gear icon area.
    pub(crate) gear_area: Option<Rect>,
}

impl Default for SplitTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SplitTable {
    pub fn new() -> Self {
        Self { sections: Vec::new(), active_section: 0, scroll_offset: 0, table_area: Rect::default(), gear_area: None }
    }

    // ── Data update ─────────���──────────────────────────────────────────

    /// Replace work-item section data (correlation-derived sections).
    ///
    /// Builds a HashMap of old sections by `SectionKind` for reuse; for each
    /// `SectionData`, if an existing section of that kind exists, updates its
    /// items (preserving selection); otherwise creates a new `SectionTable`
    /// with columns from `columns_for_section(kind)`. Empty sections are
    /// skipped. Preserves `active_section` by `SectionKind`.
    ///
    /// **Does not touch `Issues` sections that are `AnySection::Issues`** — those
    /// are managed by `update_issue_section`. If correlation-derived issue
    /// `WorkItem`s appear in `section_data`, they are treated as `WorkItems`
    /// sections (this is fine — the correlation engine can still produce
    /// issue-linked `WorkItem`s; only query-driven issues use `IssueRow`).
    pub fn update_sections(&mut self, section_data: Vec<SectionData>) {
        // Remember active kind before rebuild.
        let prev_active_kind = self.sections.get(self.active_section).map(|(k, _)| *k);

        // Drain old sections into lookups by kind.
        let mut old_work_item_sections: HashMap<SectionKind, SectionTable<WorkItem>> = HashMap::new();
        let mut old_issue_section: Option<SectionTable<IssueRow>> = None;

        for (kind, section) in self.sections.drain(..) {
            match section {
                AnySection::WorkItems(table) => {
                    old_work_item_sections.insert(kind, table);
                }
                AnySection::Issues(table) => {
                    old_issue_section = Some(table);
                }
            }
        }

        for sd in section_data {
            if sd.items.is_empty() {
                continue;
            }
            if let Some(mut existing) = old_work_item_sections.remove(&sd.kind) {
                existing.header_label = sd.label.clone();
                existing.update_items(sd.items);
                self.sections.push((sd.kind, AnySection::WorkItems(existing)));
            } else {
                let columns = columns_for_section(sd.kind);
                let mut table = SectionTable::new(sd.label.clone(), columns);
                table.update_items(sd.items);
                self.sections.push((sd.kind, AnySection::WorkItems(table)));
            }
        }

        // Re-append the issue section (if it existed) — it always goes last.
        if let Some(issue_table) = old_issue_section {
            if !issue_table.is_empty() {
                self.sections.push((SectionKind::Issues, AnySection::Issues(issue_table)));
            }
        }

        self.restore_active_section(prev_active_kind);
    }

    /// Replace the query-driven issue section with native `IssueRow` data.
    ///
    /// If a non-empty `items` vec is provided, creates or updates the
    /// `AnySection::Issues` section. If empty, removes any existing issue section.
    /// Any `WorkItems`-typed issue section from correlation is left alone.
    pub fn update_issue_section(&mut self, label: String, items: Vec<IssueRow>) {
        let prev_active_kind = self.sections.get(self.active_section).map(|(k, _)| *k);
        let mut old_issue_section = None;
        let mut retained_sections = Vec::with_capacity(self.sections.len());
        for (kind, section) in self.sections.drain(..) {
            match section {
                AnySection::Issues(table) => old_issue_section = Some(table),
                other => retained_sections.push((kind, other)),
            }
        }
        self.sections = retained_sections;

        if !items.is_empty() {
            let mut table = old_issue_section.unwrap_or_else(|| SectionTable::new(label.clone(), issue_columns_native()));
            table.header_label = label;
            table.update_items(items);
            self.sections.push((SectionKind::Issues, AnySection::Issues(table)));
        }

        self.restore_active_section(prev_active_kind);
    }

    /// Restore active section by kind after a section rebuild, and clamp.
    fn restore_active_section(&mut self, prev_active_kind: Option<SectionKind>) {
        if let Some(kind) = prev_active_kind {
            self.active_section = self.sections.iter().position(|(k, _)| *k == kind).unwrap_or(0);
        } else {
            self.active_section = 0;
        }

        if self.sections.is_empty() {
            self.active_section = 0;
        } else if self.active_section >= self.sections.len() {
            self.active_section = self.sections.len() - 1;
        }
    }

    // ── Virtual row helpers ────────────────────────────────────────────

    /// Total number of virtual rows (dividers + headers + data).
    #[cfg(test)]
    fn total_virtual_rows(&self) -> usize {
        self.sections.iter().map(|(_, s)| 2 + s.row_count()).sum()
    }

    /// Compute the flat virtual row index of the currently selected data row.
    /// Returns `None` when nothing is selected.
    fn selected_flat_row(&self) -> Option<usize> {
        let (_, section) = self.sections.get(self.active_section)?;
        let item_idx = section.selected_idx()?;
        let mut flat = 0;
        for (i, (_, s)) in self.sections.iter().enumerate() {
            if i == self.active_section {
                return Some(flat + 2 + item_idx);
            }
            flat += 2 + s.row_count();
        }
        None
    }

    /// Given a flat virtual row index, find which section and item it
    /// corresponds to. Returns `None` for divider/header rows.
    fn flat_to_section_item(&self, flat: usize) -> Option<(usize, usize)> {
        let mut offset = 0;
        for (section_idx, (_, section)) in self.sections.iter().enumerate() {
            let section_rows = 2 + section.row_count();
            if flat < offset + section_rows {
                let relative = flat - offset;
                if relative < 2 {
                    return None; // divider or header
                }
                return Some((section_idx, relative - 2));
            }
            offset += section_rows;
        }
        None
    }

    /// Adjust scroll offset so the selected row is visible.
    ///
    /// When scrolling upward, includes the section's divider and column header
    /// rows so the user always sees which section the selected item belongs to.
    fn ensure_selected_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        if let Some(flat) = self.selected_flat_row() {
            // Find the flat row of the active section's divider header.
            let section_start = self.section_start_flat(self.active_section);
            if flat < self.scroll_offset {
                // Scrolling up — show divider + column header too.
                self.scroll_offset = section_start.min(flat);
            } else if flat >= self.scroll_offset + viewport_height {
                self.scroll_offset = flat - viewport_height + 1;
            }
        }
    }

    /// The flat row index where a section's divider header begins.
    fn section_start_flat(&self, section_idx: usize) -> usize {
        let mut flat = 0;
        for (i, (_, s)) in self.sections.iter().enumerate() {
            if i == section_idx {
                return flat;
            }
            flat += 2 + s.row_count();
        }
        flat
    }

    // ── Navigation ─────────���───────────────────────────────────────────

    /// Advance selection by one row. If the active section is at its end,
    /// moves to the next section's first item.
    pub fn select_next(&mut self) {
        if self.sections.is_empty() {
            return;
        }
        let advanced = self.sections[self.active_section].1.select_next();
        if !advanced && self.active_section + 1 < self.sections.len() {
            // Cross to next section.
            self.sections[self.active_section].1.clear_selection();
            self.active_section += 1;
            self.sections[self.active_section].1.select_first();
        }
    }

    /// Retreat selection by one row. If the active section is at its start,
    /// moves to the previous section's last item.
    pub fn select_prev(&mut self) {
        if self.sections.is_empty() {
            return;
        }
        let retreated = self.sections[self.active_section].1.select_prev();
        if !retreated && self.active_section > 0 {
            // Cross to previous section.
            self.sections[self.active_section].1.clear_selection();
            self.active_section -= 1;
            self.sections[self.active_section].1.select_last();
        }
    }

    /// The currently selected row, which may be a `WorkItem` or an `IssueRow`.
    pub fn selected_row(&self) -> Option<SelectedRow<'_>> {
        self.sections.get(self.active_section).and_then(|(_, section)| section.selected_row())
    }

    /// The currently selected work item, if any. Returns `None` if the
    /// selection is in an `IssueRow` section.
    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        self.selected_row().and_then(|r| r.as_work_item())
    }

    /// Clear selection in the active section.
    pub fn clear_selection(&mut self) {
        if let Some((_, section)) = self.sections.get_mut(self.active_section) {
            section.clear_selection();
        }
    }

    /// Hit-test a mouse coordinate against the rendered virtual rows.
    /// Returns `(section_idx, item_idx)` if the point falls on a data row.
    pub fn row_at_mouse(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self.table_area;
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return None;
        }
        let relative_y = (y - area.y) as usize;
        let flat = self.scroll_offset + relative_y;
        self.flat_to_section_item(flat)
    }

    /// Set active section and select a specific item within it.
    pub fn select_by_mouse(&mut self, section_idx: usize, item_idx: usize) {
        if section_idx >= self.sections.len() {
            return;
        }
        // Clear selection in old active section.
        if self.active_section != section_idx {
            if let Some((_, section)) = self.sections.get_mut(self.active_section) {
                section.clear_selection();
            }
        }
        self.active_section = section_idx;
        self.sections[section_idx].1.select_idx(item_idx);
    }

    /// Iterate all `WorkItem`s across all sections. `IssueRow` sections are skipped.
    pub fn all_items(&self) -> impl Iterator<Item = &WorkItem> {
        self.sections.iter().flat_map(|(_, section)| match section {
            AnySection::WorkItems(t) => t.items.iter().collect::<Vec<_>>(),
            AnySection::Issues(_) => Vec::new(),
        })
    }

    /// Iterate all `WorkItem`s with `(section_idx, &WorkItem, item_idx)` tuples.
    /// `IssueRow` sections are skipped.
    pub fn all_items_with_indices(&self) -> impl Iterator<Item = (usize, &WorkItem, usize)> {
        self.sections.iter().enumerate().flat_map(|(section_idx, (_, section))| match section {
            AnySection::WorkItems(t) => t.items.iter().enumerate().map(move |(item_idx, item)| (section_idx, item, item_idx)).collect(),
            AnySection::Issues(_) => Vec::new(),
        })
    }

    /// Convenience: identity of the currently selected item. Returns `None`
    /// for `IssueRow` selections (they have no `WorkItemIdentity`).
    pub fn selected_identity(&self) -> Option<WorkItemIdentity> {
        self.selected_work_item().map(|item| item.identity.clone())
    }

    /// Total number of items across all sections (both `WorkItem` and `IssueRow`).
    pub fn total_item_count(&self) -> usize {
        self.sections.iter().map(|(_, s)| s.row_count()).sum()
    }

    /// A flat selection index across sections, for change-detection purposes.
    /// Returns `None` when nothing is selected.
    pub fn selected_flat_index(&self) -> Option<usize> {
        let selected_idx = self.sections.get(self.active_section).and_then(|(_, s)| s.selected_idx())?;
        let prior_items: usize = self.sections[..self.active_section].iter().map(|(_, s)| s.row_count()).sum();
        Some(prior_items + selected_idx)
    }

    /// Select an item by flat index (across all sections). Used by tests.
    pub fn select_flat_index(&mut self, flat_idx: usize) {
        let mut remaining = flat_idx;
        let mut target_section = None;
        for (i, (_, section)) in self.sections.iter().enumerate() {
            if remaining < section.row_count() {
                target_section = Some((i, remaining));
                break;
            }
            remaining -= section.row_count();
        }
        if let Some((section_idx, item_idx)) = target_section {
            self.select_by_mouse(section_idx, item_idx);
        }
    }

    // ── Rendering ──────────────────────────────────────────────────────

    /// Render all sections as a single scrollable surface into the given area.
    #[allow(clippy::too_many_arguments)]
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
            let close_x = area.x + area.width.saturating_sub(5);
            self.gear_area = Some(Rect::new(close_x, area.y, 3, 1));
            self.render_providers(model, ui, theme, frame, area);
            return;
        }

        // Gear icon — rendered after all rows so it overlays the top-right corner.
        let gear_x = area.x + area.width.saturating_sub(5);
        self.gear_area = Some(Rect::new(gear_x, area.y, 3, 1));

        if self.sections.is_empty() {
            frame.render_widget(Span::styled(" \u{2699} ", Style::default().fg(theme.muted)), Rect::new(gear_x, area.y, 3, 1));
            return;
        }

        let viewport_height = area.height as usize;
        self.ensure_selected_visible(viewport_height);

        // Precompute host_repo_roots from main checkouts across all sections.
        let local_repo_root = model.active_repo_root().clone();
        let mut host_repo_roots: HashMap<HostName, std::path::PathBuf> = HashMap::new();
        for (_, section) in &self.sections {
            if let AnySection::WorkItems(table) = section {
                for item in &table.items {
                    if item.is_main_checkout {
                        if let Some(co) = item.checkout_key() {
                            if let Some(host_name) = co.host_name() {
                                host_repo_roots.insert(host_name.clone(), co.path.clone());
                            }
                        }
                    }
                }
            }
        }

        // Build per-host home directories for remote path shortening.
        let host_home_dirs: HashMap<HostName, std::path::PathBuf> = model
            .hosts
            .iter()
            .filter_map(|(host, state)| state.summary.system.home_dir.as_ref().map(|d| (host.clone(), d.clone())))
            .collect();

        let providers = model.active_opt().map(|r| r.providers.as_ref());
        let default_providers = flotilla_protocol::ProviderData::default();
        let providers = providers.unwrap_or(&default_providers);
        let my_host = model.my_host();
        let selected_flat = self.selected_flat_row();

        // Available width for columns (after highlight symbol).
        let col_available = area.width.saturating_sub(HIGHLIGHT_WIDTH);

        let mut flat_row = 0usize;

        for (_, section) in &self.sections {
            // ── Divider row ──
            if flat_row >= self.scroll_offset && flat_row < self.scroll_offset + viewport_height {
                let y = area.y + (flat_row - self.scroll_offset) as u16;
                let row_rect = Rect::new(area.x, y, area.width, 1);
                render_divider(frame, section.header_label(), theme, row_rect);
            }
            flat_row += 1;

            match section {
                AnySection::WorkItems(table) => {
                    let col_widths = table.resolve_widths(col_available, COL_SPACING);

                    // ── Column header row ──
                    if flat_row >= self.scroll_offset && flat_row < self.scroll_offset + viewport_height {
                        let y = area.y + (flat_row - self.scroll_offset) as u16;
                        let row_rect = Rect::new(area.x, y, area.width, 1);
                        render_column_headers(frame, &table.columns, &col_widths, theme, row_rect);
                    }
                    flat_row += 1;

                    // ── Data rows ──
                    let mut prev_source: Option<String> = None;

                    for item in &table.items {
                        if flat_row >= self.scroll_offset && flat_row < self.scroll_offset + viewport_height {
                            let y = area.y + (flat_row - self.scroll_offset) as u16;
                            let is_selected = selected_flat == Some(flat_row);

                            let render_ctx = RenderCtx {
                                theme,
                                providers,
                                col_widths: col_widths.clone(),
                                repo_root: &local_repo_root,
                                host_repo_roots: &host_repo_roots,
                                my_host,
                                host_home_dirs: &host_home_dirs,
                                prev_source: prev_source.as_deref(),
                            };

                            let pending = pending_actions.get(&item.identity);
                            let is_multi_selected = multi_selected.contains(&item.identity);

                            render_data_row(
                                frame,
                                item,
                                &table.columns,
                                &render_ctx,
                                is_selected,
                                pending,
                                is_multi_selected,
                                y,
                                area.x,
                                area.width,
                                theme,
                            );
                        }
                        prev_source = item.source.clone();
                        flat_row += 1;
                    }
                }
                AnySection::Issues(table) => {
                    let col_widths = table.resolve_widths(col_available, COL_SPACING);

                    // ── Column header row ──
                    if flat_row >= self.scroll_offset && flat_row < self.scroll_offset + viewport_height {
                        let y = area.y + (flat_row - self.scroll_offset) as u16;
                        let row_rect = Rect::new(area.x, y, area.width, 1);
                        render_column_headers_generic(&table.columns, &col_widths, theme, frame, row_rect);
                    }
                    flat_row += 1;

                    // ── Data rows ──
                    let mut prev_source: Option<String> = None;

                    for item in &table.items {
                        if flat_row >= self.scroll_offset && flat_row < self.scroll_offset + viewport_height {
                            let y = area.y + (flat_row - self.scroll_offset) as u16;
                            let is_selected = selected_flat == Some(flat_row);

                            let render_ctx = RenderCtx {
                                theme,
                                providers,
                                col_widths: col_widths.clone(),
                                repo_root: &local_repo_root,
                                host_repo_roots: &host_repo_roots,
                                my_host,
                                host_home_dirs: &host_home_dirs,
                                prev_source: prev_source.as_deref(),
                            };

                            render_generic_data_row(frame, item, &table.columns, &render_ctx, is_selected, y, area.x, area.width, theme);
                        }
                        prev_source = Some(item.issue.provider_display_name.clone());
                        flat_row += 1;
                    }
                }
            }
        }

        // Render gear icon last so it overlays the top-right corner.
        frame.render_widget(Span::styled(" \u{2699} ", Style::default().fg(theme.muted)), Rect::new(gear_x, area.y, 3, 1));
    }

    fn render_providers(&self, model: &TuiModel, _ui: &UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        let repo_identity = &model.repo_order[model.active_repo];
        let rm = &model.repos[repo_identity];

        let mut rows: Vec<Row> = Vec::new();

        for &(category, key) in &PROVIDER_CATEGORIES {
            if let Some(pnames) = rm.provider_names.get(key) {
                for (i, pname) in pnames.iter().enumerate() {
                    let label = if i == 0 { category } else { "" };
                    let status = model.provider_statuses.get(&(repo_identity.clone(), key.to_string(), pname.clone())).copied();
                    rows.push(provider_row(label, pname, status, theme));
                }
            } else {
                rows.push(provider_empty_row(category, theme));
            }
        }

        let table = Table::new(rows, provider_table_widths())
            .header(provider_table_header(theme))
            .block(Block::bordered().style(theme.block_style()).title_top(Line::from(" \u{2715} ").right_aligned()));
        frame.render_widget(table, area);
    }
}

// ── Row rendering helpers ──────────────────────────────────────────────────

/// Render a section divider: "── Label ──"
fn render_divider(frame: &mut Frame, label: &str, theme: &Theme, area: Rect) {
    let dashes_left = 2;
    let left = "\u{2500}".repeat(dashes_left);
    let right_width = area.width.saturating_sub(dashes_left as u16 + label.chars().count() as u16 + 4);
    let right = "\u{2500}".repeat(right_width as usize);
    let header_line = Line::from(vec![
        Span::styled(format!("{left} "), Style::default().fg(theme.muted)),
        Span::styled(label.to_string(), Style::default().fg(theme.section_header)),
        Span::styled(format!(" {right}"), Style::default().fg(theme.muted)),
    ]);
    frame.render_widget(header_line, area);
}

/// Render column headers for a section.
fn render_column_headers(
    frame: &mut Frame,
    columns: &[super::section_table::ColumnDef<WorkItem>],
    col_widths: &[u16],
    theme: &Theme,
    area: Rect,
) {
    let mut spans: Vec<Span> = Vec::new();
    // Indent to match highlight symbol space.
    spans.push(Span::raw("  "));
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let w = col_widths.get(i).copied().unwrap_or(0) as usize;
        let header = &col.header;
        let padded = format!("{:<width$}", ui_helpers::truncate(header, w), width = w);
        spans.push(Span::styled(padded, Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)));
    }
    let line = Line::from(spans);
    frame.render_widget(line, area);
}

/// Render a single data row.
#[allow(clippy::too_many_arguments)]
fn render_data_row(
    frame: &mut Frame,
    item: &WorkItem,
    columns: &[super::section_table::ColumnDef<WorkItem>],
    ctx: &RenderCtx,
    is_selected: bool,
    pending: Option<&PendingAction>,
    is_multi_selected: bool,
    y: u16,
    area_x: u16,
    area_width: u16,
    theme: &Theme,
) {
    let is_in_flight = pending.is_some_and(|p| matches!(p.status, PendingStatus::InFlight));
    let is_failed = pending.is_some_and(|p| matches!(p.status, PendingStatus::Failed(_)));

    // ── In-flight shimmer rendering ────────────────────────────────────
    if is_in_flight {
        let total_width: usize = ctx.col_widths.iter().map(|w| *w as usize).sum::<usize>() + columns.len().saturating_sub(1);
        let shimmer = crate::shimmer::Shimmer::new(total_width, theme);
        let spinner = ui_helpers::spinner_char();

        let mut spans: Vec<Span> = Vec::new();
        if is_selected {
            spans.push(Span::styled(HIGHLIGHT_SYMBOL, Style::default().fg(theme.text).add_modifier(Modifier::BOLD)));
        } else {
            spans.push(Span::raw("  "));
        }

        let mut offset: usize = 0;
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
                offset += 1;
            }
            let span = (col.extract)(item, ctx);
            let w = ctx.col_widths.get(i).copied().unwrap_or(0) as usize;
            // Replace icon column with spinner.
            let text = if i == 0 { format!(" {spinner}") } else { ui_helpers::truncate(&span.content, w) };
            let padded = format!("{:<width$}", text, width = w);
            let shimmer_spans = shimmer.spans(&padded, offset);
            spans.extend(shimmer_spans);
            offset += w;
        }

        let line = Line::from(spans);
        frame.render_widget(line, Rect::new(area_x, y, area_width, 1));

        if is_selected {
            let buf = frame.buffer_mut();
            for cx in area_x..area_x + area_width {
                if let Some(cell) = buf.cell_mut(Position::new(cx, y)) {
                    cell.set_bg(theme.row_highlight);
                }
            }
        }
        return;
    }

    // ── Normal / failed / multi-selected rendering ─────────────────────
    let mut spans: Vec<Span> = Vec::new();

    // Highlight symbol.
    if is_selected {
        spans.push(Span::styled(HIGHLIGHT_SYMBOL, Style::default().fg(theme.text).add_modifier(Modifier::BOLD)));
    } else {
        spans.push(Span::raw("  "));
    }

    // Column cells.
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let span = (col.extract)(item, ctx);
        let w = ctx.col_widths.get(i).copied().unwrap_or(0) as usize;
        let truncated = ui_helpers::truncate(&span.content, w);
        let padded = format!("{:<width$}", truncated, width = w);
        if is_failed {
            spans.push(Span::styled(padded, Style::default().fg(theme.error).add_modifier(Modifier::DIM)));
        } else {
            spans.push(Span::styled(padded, span.style));
        }
    }

    // Failed icon override.
    if is_failed {
        spans[0] =
            if is_selected { Span::styled("\u{2717} ", Style::default().fg(theme.error)) } else { Span::styled("  ", Style::default()) };
    }

    let line = Line::from(spans);
    frame.render_widget(line, Rect::new(area_x, y, area_width, 1));

    // Background layers: multi-select first, then row highlight on top for the
    // active row so it remains visually distinct within the selection.
    if is_multi_selected {
        let buf = frame.buffer_mut();
        for cx in area_x..area_x + area_width {
            if let Some(cell) = buf.cell_mut(Position::new(cx, y)) {
                cell.set_bg(theme.multi_select_bg);
            }
        }
    }
    if is_selected {
        let buf = frame.buffer_mut();
        for cx in area_x..area_x + area_width {
            if let Some(cell) = buf.cell_mut(Position::new(cx, y)) {
                cell.set_bg(theme.row_highlight);
            }
        }
    }
}

// ── Generic row rendering helpers (for IssueRow and other future types) ────

/// Render column headers for any `ColumnDef<T>` section.
fn render_column_headers_generic<T>(columns: &[ColumnDef<T>], col_widths: &[u16], theme: &Theme, frame: &mut Frame, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw("  "));
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let w = col_widths.get(i).copied().unwrap_or(0) as usize;
        let header = &col.header;
        let padded = format!("{:<width$}", ui_helpers::truncate(header, w), width = w);
        spans.push(Span::styled(padded, Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)));
    }
    let line = Line::from(spans);
    frame.render_widget(line, area);
}

/// Render a single data row for any `ColumnDef<T>` section (no pending/multi-select support).
#[allow(clippy::too_many_arguments)]
fn render_generic_data_row<T>(
    frame: &mut Frame,
    item: &T,
    columns: &[ColumnDef<T>],
    ctx: &RenderCtx,
    is_selected: bool,
    y: u16,
    area_x: u16,
    area_width: u16,
    theme: &Theme,
) {
    let mut spans: Vec<Span> = Vec::new();

    if is_selected {
        spans.push(Span::styled(HIGHLIGHT_SYMBOL, Style::default().fg(theme.text).add_modifier(Modifier::BOLD)));
    } else {
        spans.push(Span::raw("  "));
    }

    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let span = (col.extract)(item, ctx);
        let w = ctx.col_widths.get(i).copied().unwrap_or(0) as usize;
        let truncated = ui_helpers::truncate(&span.content, w);
        let padded = format!("{:<width$}", truncated, width = w);
        spans.push(Span::styled(padded, span.style));
    }

    let line = Line::from(spans);
    frame.render_widget(line, Rect::new(area_x, y, area_width, 1));

    if is_selected {
        let buf = frame.buffer_mut();
        for cx in area_x..area_x + area_width {
            if let Some(cell) = buf.cell_mut(Position::new(cx, y)) {
                cell.set_bg(theme.row_highlight);
            }
        }
    }
}

// ── Provider table helpers ────────────────────────────────────────────────

fn provider_status_badge(status: Option<ProviderStatus>, theme: &Theme) -> (&'static str, Color) {
    match status {
        Some(ProviderStatus::Ok) => ("\u{2713}", theme.status_ok),
        Some(ProviderStatus::Error) => ("\u{2717}", theme.error),
        None => ("", theme.text),
    }
}

fn provider_row(label: &str, provider: &str, status: Option<ProviderStatus>, theme: &Theme) -> Row<'static> {
    let (status_text, status_color) = provider_status_badge(status, theme);
    Row::new(vec![
        Cell::from(Span::styled(label.to_string(), Style::default().fg(theme.muted))),
        Cell::from(Span::styled(provider.to_string(), Style::default().fg(theme.text))),
        Cell::from(Span::styled(status_text, Style::default().fg(status_color))),
    ])
}

fn provider_empty_row(category: &str, theme: &Theme) -> Row<'static> {
    Row::new(vec![
        Cell::from(Span::styled(category.to_string(), Style::default().fg(theme.muted))),
        Cell::from(Span::styled("\u{2014}", Style::default().fg(theme.muted))),
        Cell::from(""),
    ])
}

fn provider_table_header(theme: &Theme) -> Row<'static> {
    Row::new(vec![
        Cell::from(Span::styled("Role", Style::default().fg(theme.muted).bold())),
        Cell::from(Span::styled("Provider", Style::default().fg(theme.muted).bold())),
        Cell::from(Span::styled("Status", Style::default().fg(theme.muted).bold())),
    ])
    .height(1)
}

fn provider_table_widths() -> [Constraint; 3] {
    [Constraint::Length(16), Constraint::Length(24), Constraint::Length(6)]
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use flotilla_protocol::{provider_data::Issue, HostPath, NodeId, WorkItemKind};

    use super::*;

    // -- Test helpers --------------------------------------------------------

    fn test_work_item(kind: WorkItemKind, id: &str) -> WorkItem {
        WorkItem {
            kind: kind.clone(),
            identity: WorkItemIdentity::Issue(id.into()),
            node_id: NodeId::new("localhost"),
            branch: None,
            description: format!("Item {id}"),
            checkout: None,
            change_request_key: None,
            session_key: None,
            issue_keys: if kind == WorkItemKind::Issue { vec![id.into()] } else { vec![] },
            workspace_refs: vec![],
            is_main_checkout: false,
            debug_group: vec![],
            source: None,
            terminal_keys: vec![],
            attachable_set_id: None,
            agent_keys: vec![],
        }
    }

    fn checkout_work_item(id: &str) -> WorkItem {
        WorkItem {
            kind: WorkItemKind::Checkout,
            identity: WorkItemIdentity::Checkout(HostPath::new(flotilla_protocol::HostName::new("localhost"), format!("/tmp/{id}")).into()),
            node_id: NodeId::new("localhost"),
            branch: Some(format!("branch-{id}")),
            description: format!("Checkout {id}"),
            checkout: None,
            change_request_key: None,
            session_key: None,
            issue_keys: vec![],
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
            items: ids.iter().map(|id| test_work_item(WorkItemKind::Issue, id)).collect(),
        }
    }

    fn native_issue_row(id: &str) -> IssueRow {
        IssueRow {
            id: id.to_string(),
            issue: Issue {
                title: format!("Issue {id}"),
                labels: vec![],
                association_keys: vec![],
                provider_name: "github".to_string(),
                provider_display_name: "GitHub".to_string(),
            },
        }
    }

    fn checkout_section(ids: &[&str]) -> SectionData {
        SectionData {
            kind: SectionKind::Checkouts,
            label: "Checkouts".into(),
            items: ids.iter().map(|id| checkout_work_item(id)).collect(),
        }
    }

    // -- Tests ---------------------------------------------------------------

    #[test]
    fn cross_section_navigation_next() {
        let mut st = SplitTable::new();
        st.update_sections(vec![checkout_section(&["a", "b"]), issue_section(&["1", "2"])]);

        assert_eq!(st.sections.len(), 2);
        assert_eq!(st.active_section, 0);

        // Start at first item of first section.
        assert!(st.selected_work_item().is_some());
        assert_eq!(st.selected_work_item().expect("selected").description, "Checkout a");

        // Advance within first section.
        st.select_next();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Checkout b");

        // Advance past end of first section -> crosses to second section.
        st.select_next();
        assert_eq!(st.active_section, 1);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");

        // Advance within second section.
        st.select_next();
        assert_eq!(st.active_section, 1);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 2");
    }

    #[test]
    fn cross_section_navigation_prev() {
        let mut st = SplitTable::new();
        st.update_sections(vec![checkout_section(&["a", "b"]), issue_section(&["1", "2"])]);

        // Move to last item: advance 3 times from start.
        st.select_next(); // Checkout b
        st.select_next(); // Issue 1 (cross)
        st.select_next(); // Issue 2
        assert_eq!(st.active_section, 1);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 2");

        // Retreat within second section.
        st.select_prev();
        assert_eq!(st.active_section, 1);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");

        // Retreat past start of second section -> crosses to first section's last item.
        st.select_prev();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Checkout b");

        // Retreat within first section.
        st.select_prev();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Checkout a");
    }

    #[test]
    fn update_sections_preserves_selection() {
        let mut st = SplitTable::new();
        st.update_sections(vec![issue_section(&["1", "2", "3"])]);

        // Select item "2".
        st.select_next();
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 2");

        // Reorder items: "2" is now at a different position.
        st.update_sections(vec![issue_section(&["3", "1", "2"])]);

        // Selection should follow item "2" to its new position.
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 2");
    }

    #[test]
    fn update_issue_section_preserves_selection_when_appending_page() {
        let mut st = SplitTable::new();
        st.update_issue_section("Issues".into(), vec![native_issue_row("1"), native_issue_row("2"), native_issue_row("3")]);

        st.select_next();
        assert_eq!(st.selected_row().and_then(|row| row.as_issue_row()).expect("selected issue").id, "2");

        st.update_issue_section("Issues".into(), vec![
            native_issue_row("1"),
            native_issue_row("2"),
            native_issue_row("3"),
            native_issue_row("4"),
            native_issue_row("5"),
        ]);

        assert_eq!(st.selected_row().and_then(|row| row.as_issue_row()).expect("selected issue").id, "2");
    }

    #[test]
    fn empty_sections_omitted() {
        let mut st = SplitTable::new();
        let empty_issues = SectionData { kind: SectionKind::Issues, label: "Issues".into(), items: vec![] };
        st.update_sections(vec![checkout_section(&["a"]), empty_issues, issue_section(&["1"])]);

        // The empty issues section should not be stored. But note: there are
        // two SectionKind::Issues entries — the empty one is skipped, the
        // non-empty one is kept.
        assert_eq!(st.sections.len(), 2);
        assert_eq!(st.sections[0].0, SectionKind::Checkouts);
        assert_eq!(st.sections[1].0, SectionKind::Issues);
    }

    #[test]
    fn select_next_stays_at_end_of_last_section() {
        let mut st = SplitTable::new();
        st.update_sections(vec![issue_section(&["1"])]);

        // Already at the only item.
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");

        // Trying to advance should stay put.
        st.select_next();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");

        // Repeated attempts also stay put.
        st.select_next();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");
    }

    #[test]
    fn select_prev_stays_at_start_of_first_section() {
        let mut st = SplitTable::new();
        st.update_sections(vec![issue_section(&["1"])]);

        // Already at the only item.
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");

        // Trying to retreat should stay put.
        st.select_prev();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");

        // Repeated attempts also stay put.
        st.select_prev();
        assert_eq!(st.active_section, 0);
        assert_eq!(st.selected_work_item().expect("selected").description, "Item 1");
    }

    #[test]
    fn update_sections_preserves_active_section_by_kind() {
        let mut st = SplitTable::new();
        st.update_sections(vec![checkout_section(&["a"]), issue_section(&["1", "2"])]);

        // Navigate to Issues section.
        st.select_next(); // crosses to Issues
        st.select_next();
        assert_eq!(st.active_section, 1);
        assert_eq!(st.sections[st.active_section].0, SectionKind::Issues);

        // Update sections — Issues section should remain active.
        st.update_sections(vec![checkout_section(&["a", "b"]), issue_section(&["1", "2", "3"])]);

        assert_eq!(st.sections[st.active_section].0, SectionKind::Issues);
    }

    #[test]
    fn total_virtual_rows() {
        let mut st = SplitTable::new();
        st.update_sections(vec![checkout_section(&["a", "b"]), issue_section(&["1"])]);

        // Section 0: 1 divider + 1 header + 2 data = 4
        // Section 1: 1 divider + 1 header + 1 data = 3
        assert_eq!(st.total_virtual_rows(), 7);
    }

    #[test]
    fn selected_flat_row_and_flat_to_section() {
        let mut st = SplitTable::new();
        st.update_sections(vec![checkout_section(&["a", "b"]), issue_section(&["1"])]);

        // Initial: section 0, item 0 -> flat row 2 (divider=0, header=1, data=2)
        assert_eq!(st.selected_flat_row(), Some(2));

        // Navigate to section 0, item 1 -> flat row 3
        st.select_next();
        assert_eq!(st.selected_flat_row(), Some(3));

        // Navigate to section 1, item 0 -> flat row 6 (section 0: 4 rows; section 1: divider=4, header=5, data=6)
        st.select_next();
        assert_eq!(st.selected_flat_row(), Some(6));

        // Verify flat_to_section_item
        assert_eq!(st.flat_to_section_item(0), None); // divider
        assert_eq!(st.flat_to_section_item(1), None); // header
        assert_eq!(st.flat_to_section_item(2), Some((0, 0))); // data
        assert_eq!(st.flat_to_section_item(3), Some((0, 1))); // data
        assert_eq!(st.flat_to_section_item(4), None); // divider
        assert_eq!(st.flat_to_section_item(5), None); // header
        assert_eq!(st.flat_to_section_item(6), Some((1, 0))); // data
        assert_eq!(st.flat_to_section_item(7), None); // beyond
    }

    #[test]
    fn ensure_selected_visible_scrolls_down() {
        let mut st = SplitTable::new();
        // 10 items -> 12 virtual rows (1 divider + 1 header + 10 data)
        let ids: Vec<&str> = vec!["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"];
        st.update_sections(vec![issue_section(&ids)]);

        // Navigate to last item (flat row 11).
        for _ in 0..9 {
            st.select_next();
        }
        assert_eq!(st.selected_flat_row(), Some(11));

        // With viewport of 5, ensure_selected_visible should scroll.
        st.ensure_selected_visible(5);
        assert_eq!(st.scroll_offset, 7); // 11 - 5 + 1 = 7
    }
}
