use std::collections::{HashMap, HashSet};

use flotilla_core::data::{SectionData, SectionKind};
use flotilla_protocol::{WorkItem, WorkItemIdentity};
use ratatui::{
    layout::{Constraint, Layout, Rect},
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
// SplitTable
// ---------------------------------------------------------------------------

/// Composes multiple `SectionTable<WorkItem>` instances — one per section kind.
/// Handles cross-section navigation, height allocation, rendering of section
/// divider headers, and exposes the same `selected_work_item()` API that
/// `RepoPage` expects.
pub struct SplitTable {
    /// Ordered sections with their kind. Only non-empty sections present.
    sections: Vec<(SectionKind, SectionTable<WorkItem>)>,
    /// Which section currently has focus (index into sections).
    active_section: usize,
    /// Stored from render for mouse hit-testing.
    pub(crate) table_area: Rect,
    /// Per-section rendered areas, for mouse dispatch.
    section_areas: Vec<Rect>,
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
        Self { sections: Vec::new(), active_section: 0, table_area: Rect::default(), section_areas: Vec::new(), gear_area: None }
    }

    // ── Data update ────────────────────────────────────────────────────

    /// Replace section data.
    ///
    /// Builds a HashMap of old sections by `SectionKind` for reuse; for each
    /// `SectionData`, if an existing section of that kind exists, updates its
    /// items (preserving selection); otherwise creates a new `SectionTable`
    /// with columns from `columns_for_section(kind)`. Empty sections are
    /// skipped. Clamps `active_section` if it is now out of bounds.
    pub fn update_sections(&mut self, section_data: Vec<SectionData>) {
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

        // Clamp active section.
        if self.sections.is_empty() {
            self.active_section = 0;
        } else if self.active_section >= self.sections.len() {
            self.active_section = self.sections.len() - 1;
        }
    }

    // ── Navigation ─────────────────────────────────────────────────────

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

    /// Hit-test a mouse coordinate against rendered section areas.
    /// Returns `(section_idx, item_idx)` if the point falls on a data row.
    pub fn row_at_mouse(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        for (section_idx, area) in self.section_areas.iter().enumerate() {
            if x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height {
                if let Some(item_idx) = self.sections[section_idx].1.row_at_y(y, *area) {
                    return Some((section_idx, item_idx));
                }
            }
        }
        None
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

    /// Render all sections into the given area.
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

        // Gear icon in top-right corner.
        let gear_x = area.x + area.width.saturating_sub(5);
        self.gear_area = Some(Rect::new(gear_x, area.y, 3, 1));

        if self.sections.is_empty() {
            self.section_areas.clear();
            return;
        }

        // ── Height allocation ──────────────────────────────────────────
        //
        // Each section gets 1 line for its divider header + a proportional
        // share of remaining space based on item count. Minimum 3 rows for
        // the table part (column header + border + at least 1 data row).

        let section_count = self.sections.len();
        let divider_lines = section_count as u16;
        let remaining = area.height.saturating_sub(divider_lines);
        let total_items: usize = self.sections.iter().map(|(_, t)| t.items.len()).sum();

        let mut constraints: Vec<Constraint> = Vec::with_capacity(section_count * 2);
        let mut table_heights: Vec<u16> = Vec::with_capacity(section_count);

        for (_, table) in &self.sections {
            let proportional = if total_items > 0 {
                ((table.items.len() as u64 * remaining as u64) / total_items as u64) as u16
            } else {
                remaining / section_count as u16
            };
            let table_h = proportional.max(3);
            table_heights.push(table_h);
            // 1 line for divider header.
            constraints.push(Constraint::Length(1));
            // Proportional height for the table.
            constraints.push(Constraint::Length(table_h));
        }

        let chunks = Layout::vertical(constraints).split(area);

        // ── Render each section ────────────────────────────────────────

        let providers = model.active_opt().map(|r| r.providers.as_ref());
        let default_providers = flotilla_protocol::ProviderData::default();
        let providers = providers.unwrap_or(&default_providers);

        self.section_areas.clear();

        // Build row-style override closure for multi-select and pending-action indicators.
        let row_style = |item: &WorkItem| -> Option<Style> {
            if let Some(pending) = pending_actions.get(&item.identity) {
                match &pending.status {
                    PendingStatus::Failed(_) => Some(Style::default().fg(theme.error).add_modifier(Modifier::DIM)),
                    PendingStatus::InFlight => None,
                }
            } else if multi_selected.contains(&item.identity) {
                Some(Style::default().bg(theme.multi_select_bg))
            } else {
                None
            }
        };

        for (i, (_kind, table)) in self.sections.iter_mut().enumerate() {
            let divider_area = chunks[i * 2];
            let table_area = chunks[i * 2 + 1];

            // Render section divider header: "── Label ──"
            let label = &table.header_label;
            let dashes_left = 2;
            let left = "\u{2500}".repeat(dashes_left);
            let right_width = divider_area.width.saturating_sub(dashes_left as u16 + label.len() as u16 + 4);
            let right = "\u{2500}".repeat(right_width as usize);
            let header_line = Line::from(vec![
                Span::styled(format!("{left} "), Style::default().fg(theme.muted)),
                Span::styled(label.clone(), Style::default().fg(theme.section_header)),
                Span::styled(format!(" {right}"), Style::default().fg(theme.muted)),
            ]);
            frame.render_widget(header_line, divider_area);

            // Compute column widths for RenderCtx.
            let col_widths: Vec<u16> = table
                .columns
                .iter()
                .map(|c| match c.width {
                    Constraint::Length(n) => n,
                    Constraint::Fill(_) => table_area.width / table.columns.len().max(1) as u16,
                    _ => 10,
                })
                .collect();

            let render_ctx = RenderCtx { theme, providers, col_widths };

            let highlight_style = if i == self.active_section { Style::default().bg(theme.row_highlight) } else { Style::default() };

            table.render(frame, table_area, &render_ctx, highlight_style, Some(&row_style));
            self.section_areas.push(table_area);
        }
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

// ── Provider table helpers ──────────────────────────────────────────────────

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
}
