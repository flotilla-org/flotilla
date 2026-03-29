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
    columns::columns_for_section,
    section_table::{Identifiable, RenderCtx, SectionTable},
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
// SplitTable
// ---------------------------------------------------------------------------

/// Composes multiple `SectionTable<WorkItem>` instances — one per section kind.
/// Renders them as a single scrollable surface with one flat cursor.
///
/// Virtual row layout per section:
///   1 divider row + 1 column header row + N data rows
///
/// Only data rows are selectable.
pub struct SplitTable {
    /// Ordered sections with their kind. Only non-empty sections present.
    sections: Vec<(SectionKind, SectionTable<WorkItem>)>,
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

    /// Replace section data.
    ///
    /// Builds a HashMap of old sections by `SectionKind` for reuse; for each
    /// `SectionData`, if an existing section of that kind exists, updates its
    /// items (preserving selection); otherwise creates a new `SectionTable`
    /// with columns from `columns_for_section(kind)`. Empty sections are
    /// skipped. Preserves `active_section` by `SectionKind`.
    pub fn update_sections(&mut self, section_data: Vec<SectionData>) {
        // Remember active kind before rebuild.
        let prev_active_kind = self.sections.get(self.active_section).map(|(k, _)| *k);

        // Drain old sections into a lookup by kind.
        let mut old_sections: HashMap<SectionKind, SectionTable<WorkItem>> = self.sections.drain(..).collect();

        for sd in section_data {
            if sd.items.is_empty() {
                continue;
            }
            if let Some(mut existing) = old_sections.remove(&sd.kind) {
                existing.header_label = sd.label.clone();
                existing.update_items(sd.items);
                self.sections.push((sd.kind, existing));
            } else {
                let columns = columns_for_section(sd.kind);
                let mut table = SectionTable::new(sd.label.clone(), columns);
                table.update_items(sd.items);
                self.sections.push((sd.kind, table));
            }
        }

        // Restore active section by kind.
        if let Some(kind) = prev_active_kind {
            self.active_section = self.sections.iter().position(|(k, _)| *k == kind).unwrap_or(0);
        } else {
            self.active_section = 0;
        }

        // Clamp active section.
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
        self.sections.iter().map(|(_, t)| 2 + t.items.len()).sum()
    }

    /// Compute the flat virtual row index of the currently selected data row.
    /// Returns `None` when nothing is selected.
    fn selected_flat_row(&self) -> Option<usize> {
        let (_, table) = self.sections.get(self.active_section)?;
        let item_idx = table.selected_idx?;
        let mut flat = 0;
        for (i, (_, t)) in self.sections.iter().enumerate() {
            if i == self.active_section {
                return Some(flat + 2 + item_idx);
            }
            flat += 2 + t.items.len();
        }
        None
    }

    /// Given a flat virtual row index, find which section and item it
    /// corresponds to. Returns `None` for divider/header rows.
    fn flat_to_section_item(&self, flat: usize) -> Option<(usize, usize)> {
        let mut offset = 0;
        for (section_idx, (_, table)) in self.sections.iter().enumerate() {
            let section_rows = 2 + table.items.len();
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
        for (i, (_, t)) in self.sections.iter().enumerate() {
            if i == section_idx {
                return flat;
            }
            flat += 2 + t.items.len();
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

    /// The currently selected work item, if any.
    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        self.sections.get(self.active_section).and_then(|(_, table)| table.selected_item())
    }

    /// Clear selection in the active section.
    pub fn clear_selection(&mut self) {
        if let Some((_, table)) = self.sections.get_mut(self.active_section) {
            table.clear_selection();
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
            if let Some((_, table)) = self.sections.get_mut(self.active_section) {
                table.clear_selection();
            }
        }
        self.active_section = section_idx;
        self.sections[section_idx].1.select_idx(item_idx);
    }

    /// Iterate all items across all sections.
    pub fn all_items(&self) -> impl Iterator<Item = &WorkItem> {
        self.sections.iter().flat_map(|(_, table)| table.items.iter())
    }

    /// Iterate all items with `(section_idx, &WorkItem, item_idx)` tuples.
    pub fn all_items_with_indices(&self) -> impl Iterator<Item = (usize, &WorkItem, usize)> {
        self.sections
            .iter()
            .enumerate()
            .flat_map(|(section_idx, (_, table))| table.items.iter().enumerate().map(move |(item_idx, item)| (section_idx, item, item_idx)))
    }

    /// Convenience: identity of the currently selected item.
    pub fn selected_identity(&self) -> Option<WorkItemIdentity> {
        self.selected_work_item().map(|item| item.identity.clone())
    }

    /// Total number of items across all sections.
    pub fn total_item_count(&self) -> usize {
        self.sections.iter().map(|(_, table)| table.items.len()).sum()
    }

    /// A flat selection index across sections, for change-detection purposes.
    /// Returns `None` when nothing is selected.
    pub fn selected_flat_index(&self) -> Option<usize> {
        let selected_idx = self.sections.get(self.active_section).and_then(|(_, t)| t.selected_idx)?;
        let prior_items: usize = self.sections[..self.active_section].iter().map(|(_, t)| t.items.len()).sum();
        Some(prior_items + selected_idx)
    }

    /// Select an item by flat index (across all sections). Used by tests.
    pub fn select_flat_index(&mut self, flat_idx: usize) {
        let mut remaining = flat_idx;
        let mut target_section = None;
        for (i, (_, table)) in self.sections.iter().enumerate() {
            if remaining < table.items.len() {
                target_section = Some((i, remaining));
                break;
            }
            remaining -= table.items.len();
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
        for (_, table) in &self.sections {
            for item in &table.items {
                if item.is_main_checkout {
                    if let Some(co) = item.checkout_key() {
                        host_repo_roots.insert(co.host.clone(), co.path.clone());
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

        for (_, table) in &self.sections {
            // ── Divider row ──
            if flat_row >= self.scroll_offset && flat_row < self.scroll_offset + viewport_height {
                let y = area.y + (flat_row - self.scroll_offset) as u16;
                let row_rect = Rect::new(area.x, y, area.width, 1);
                render_divider(frame, &table.header_label, theme, row_rect);
            }
            flat_row += 1;

            // Resolve column widths for this section.
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

// ── Provider table helpers ─��────────────────────────────────────────────────

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
    use flotilla_protocol::{HostPath, WorkItemKind};

    use super::*;

    // -- Test helpers --------------------------------------------------------

    fn test_work_item(kind: WorkItemKind, id: &str) -> WorkItem {
        WorkItem {
            kind: kind.clone(),
            identity: WorkItemIdentity::Issue(id.into()),
            host: flotilla_protocol::HostName::new("localhost"),
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
            identity: WorkItemIdentity::Checkout(HostPath::new(flotilla_protocol::HostName::new("localhost"), format!("/tmp/{id}"))),
            host: flotilla_protocol::HostName::new("localhost"),
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
