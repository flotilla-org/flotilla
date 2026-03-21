use std::{
    any::Any,
    collections::HashMap,
    path::{Path, PathBuf},
};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::data::{GroupEntry, GroupedWorkItems, SectionHeader};
use flotilla_protocol::{HostName, ProviderData, WorkItem};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, HighlightSpacing, Row, Table, TableState},
    Frame,
};

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::{
        ui_state::{BranchInputKind, PendingAction, PendingStatus, UiMode},
        ProviderStatus, TuiModel, UiState,
    },
    keymap::{Action, ModeId},
    shimmer::Shimmer,
    theme::Theme,
    ui_helpers,
};

const HIGHLIGHT_SYMBOL: &str = "▸ ";
const HIGHLIGHT_SYMBOL_WIDTH: u16 = 2;
const PROVIDER_CATEGORIES: [(&str, &str); 8] = [
    ("VCS", "vcs"),
    ("Checkout mgr", "checkout_manager"),
    ("Change request", "change_request"),
    ("Issue tracker", "issue_tracker"),
    ("Cloud agents", "cloud_agent"),
    ("AI utility", "ai_utility"),
    ("Workspace mgr", "workspace_manager"),
    ("Terminal pool", "terminal_pool"),
];

/// The work-item table component. Owned by `RepoPage` and implements
/// `InteractiveWidget` for uniform action/mouse/render dispatch.
pub struct WorkItemTable {
    /// Stored from render for mouse hit-testing.
    pub(crate) table_area: Rect,
    /// Gear icon area, captured from layout after table render.
    pub(crate) gear_area: Option<Rect>,
    // ── Owned selection state (for future RepoPage use) ──────────────
    /// Ratatui table widget state (tracks selected row and scroll offset).
    pub table_state: TableState,
    /// Index into `grouped_items.selectable_indices` for the currently
    /// highlighted row, or `None` when the table is empty.
    pub selected_selectable_idx: Option<usize>,
    /// The grouped work items displayed in this table.
    pub grouped_items: GroupedWorkItems,
}

impl WorkItemTable {
    pub fn new() -> Self {
        Self {
            table_area: Rect::default(),
            gear_area: None,
            table_state: TableState::default(),
            selected_selectable_idx: None,
            grouped_items: GroupedWorkItems::default(),
        }
    }

    // ── Owned data + selection ──────────────────────────────────────

    /// Replace the grouped items and restore selection by work item identity.
    ///
    /// After the update, if the previously-selected item still exists in the
    /// new data it remains selected. Otherwise selection falls back to the
    /// first selectable row, or `None` if the table is empty.
    pub fn update_items(&mut self, items: GroupedWorkItems) {
        let prev_identity =
            self.selected_selectable_idx.and_then(|si| self.grouped_items.selectable_indices.get(si).copied()).and_then(|ti| {
                match self.grouped_items.table_entries.get(ti) {
                    Some(GroupEntry::Item(item)) => Some(item.identity.clone()),
                    _ => None,
                }
            });

        self.grouped_items = items;

        if self.grouped_items.selectable_indices.is_empty() {
            self.selected_selectable_idx = None;
            self.table_state.select(None);
        } else if let Some(ref identity) = prev_identity {
            let found = self.grouped_items.selectable_indices.iter().enumerate().find(|(_, &ti)| {
                matches!(
                    self.grouped_items.table_entries.get(ti),
                    Some(GroupEntry::Item(item)) if item.identity == *identity
                )
            });
            if let Some((si, &ti)) = found {
                self.selected_selectable_idx = Some(si);
                self.table_state.select(Some(ti));
            } else {
                self.selected_selectable_idx = Some(0);
                self.table_state.select(Some(self.grouped_items.selectable_indices[0]));
            }
        } else {
            self.selected_selectable_idx = Some(0);
            self.table_state.select(Some(self.grouped_items.selectable_indices[0]));
        }
    }

    /// Move selection down one row using owned state.
    pub fn select_next_self(&mut self) {
        let indices = &self.grouped_items.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let next = match self.selected_selectable_idx {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = indices[next];
        self.selected_selectable_idx = Some(next);
        self.table_state.select(Some(table_idx));
    }

    /// Move selection up one row using owned state.
    pub fn select_prev_self(&mut self) {
        let indices = &self.grouped_items.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let prev = match self.selected_selectable_idx {
            Some(si) if si > 0 => si - 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = indices[prev];
        self.selected_selectable_idx = Some(prev);
        self.table_state.select(Some(table_idx));
    }

    /// Hit-test a mouse position using owned table state (no ctx needed).
    pub fn row_at_mouse_self(&self, x: u16, y: u16) -> Option<usize> {
        let ta = self.table_area;
        if x >= ta.x && x < ta.x + ta.width && y >= ta.y && y < ta.y + ta.height {
            let row_in_table = (y - ta.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let offset = self.table_state.offset();
            let actual_row = data_row + offset;
            self.grouped_items.selectable_indices.iter().position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    /// Select a row by selectable index using owned state.
    pub fn select_row_self(&mut self, si: usize) {
        if let Some(&table_idx) = self.grouped_items.selectable_indices.get(si) {
            self.selected_selectable_idx = Some(si);
            self.table_state.select(Some(table_idx));
        }
    }

    // ── Selection helpers (ctx-based, for current callers) ───────────

    pub fn select_next(&self, ctx: &mut WidgetContext) {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        let indices = &rui.table_view.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = rui.selected_selectable_idx;
        let next = match current_si {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = rui.table_view.selectable_indices[next];
        rui.selected_selectable_idx = Some(next);
        rui.table_state.select(Some(table_idx));
    }

    pub fn select_prev(&self, ctx: &mut WidgetContext) {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        let indices = &rui.table_view.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = rui.selected_selectable_idx;
        let prev = match current_si {
            Some(si) if si > 0 => si - 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = rui.table_view.selectable_indices[prev];
        rui.selected_selectable_idx = Some(prev);
        rui.table_state.select(Some(table_idx));
    }

    pub fn toggle_multi_select(&self, ctx: &mut WidgetContext) {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        if let Some(si) = rui.selected_selectable_idx {
            if let Some(&table_idx) = rui.table_view.selectable_indices.get(si) {
                if let Some(GroupEntry::Item(item)) = rui.table_view.table_entries.get(table_idx) {
                    let identity = item.identity.clone();
                    if !rui.multi_selected.remove(&identity) {
                        rui.multi_selected.insert(identity);
                    }
                }
            }
        }
    }

    // ── Rendering ────────────────────────────────────────────────────

    fn render_table(&mut self, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        ui.layout.table_area = area;

        let rui = active_rui(model, ui);
        if rui.show_providers {
            let close_x = area.x + area.width.saturating_sub(5);
            self.gear_area = Some(Rect::new(close_x, area.y, 3, 1));
            self.render_providers(model, ui, theme, frame, area);
            return;
        }

        let gear_x = area.x + area.width.saturating_sub(5);
        self.gear_area = Some(Rect::new(gear_x, area.y, 3, 1));

        let labels = model.active_labels();
        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("Source"),
            Cell::from("Path"),
            Cell::from("Description"),
            Cell::from("Branch"),
            Cell::from(labels.checkouts.abbr.as_str()),
            Cell::from("WS"),
            Cell::from(labels.change_requests.abbr.as_str()),
            Cell::from(labels.cloud_agents.abbr.as_str()),
            Cell::from("Issues"),
            Cell::from("Git"),
        ])
        .style(Style::default().fg(theme.muted).bold())
        .height(1);

        let widths = [
            Constraint::Length(3),  // icon
            Constraint::Length(10), // Source
            Constraint::Fill(1),    // Path
            Constraint::Fill(2),    // Description
            Constraint::Fill(1),    // Branch
            Constraint::Length(3),  // WT
            Constraint::Length(3),  // WS
            Constraint::Length(4),  // PR
            Constraint::Length(4),  // SS
            Constraint::Length(6),  // Issues
            Constraint::Length(5),  // Git
        ];

        let inner_width = area.width.saturating_sub(2 + HIGHLIGHT_SYMBOL_WIDTH);
        let col_areas = Layout::horizontal(widths).split(Rect::new(0, 0, inner_width, 1));
        let col_widths: Vec<u16> = col_areas.iter().map(|r| r.width).collect();

        // Build rows from active repo (immutable borrows)
        let rm = model.active();
        let rui = active_rui(model, ui);

        // Precompute per-host repo root from main checkouts so remote worktree
        // paths get the same sibling/child indentation as local ones.
        let local_repo_root = model.active_repo_root().clone();
        let mut host_repo_roots: HashMap<HostName, PathBuf> = HashMap::new();
        for entry in &rui.table_view.table_entries {
            if let GroupEntry::Item(item) = entry {
                if item.is_main_checkout {
                    if let Some(co) = item.checkout_key() {
                        host_repo_roots.insert(co.host.clone(), co.path.clone());
                    }
                }
            }
        }

        let mut prev_source: Option<String> = None;
        let rows: Vec<Row> = rui
            .table_view
            .table_entries
            .iter()
            .map(|entry| {
                let is_multi_selected =
                    if let GroupEntry::Item(ref item) = entry { rui.multi_selected.contains(&item.identity) } else { false };

                match entry {
                    GroupEntry::Header(header) => {
                        prev_source = None;
                        build_header_row(header)
                    }
                    GroupEntry::Item(item) => {
                        let pending = rui.pending_actions.get(&item.identity);
                        let is_local_item = item
                            .checkout_key()
                            .is_none_or(|co| model.my_host().is_some_and(|my| *my == co.host) || !model.hosts.contains_key(&co.host));
                        let local_home = if is_local_item { dirs::home_dir() } else { None };
                        let home_dir = item
                            .checkout_key()
                            .and_then(|co| model.hosts.get(&co.host))
                            .and_then(|h| h.summary.system.home_dir.as_deref())
                            .or(local_home.as_deref());
                        let repo_root = item.checkout_key().and_then(|co| host_repo_roots.get(&co.host)).unwrap_or(&local_repo_root);
                        let mut row =
                            build_item_row(item, &rm.providers, &col_widths, repo_root, prev_source.as_deref(), pending, theme, home_dir);
                        prev_source = item.source.clone();
                        if is_multi_selected {
                            row = row.style(Style::default().bg(theme.multi_select_bg));
                        }
                        row
                    }
                }
            })
            .collect();

        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().style(theme.block_style()).title_top(Line::from(" ⚙ ").right_aligned()))
            .row_highlight_style(Style::default().bg(theme.row_highlight).bold())
            .highlight_symbol(HIGHLIGHT_SYMBOL)
            .highlight_spacing(HighlightSpacing::Always);

        // Now mutably borrow for stateful render
        let key = &model.repo_order[model.active_repo];
        let rui = ui.repo_ui.get_mut(key).expect("active repo must have UI state");
        frame.render_stateful_widget(table, area, &mut rui.table_state);

        // Overlay section headers so they span the full row width
        let offset = rui.table_state.offset();
        let visible_rows = area.height.saturating_sub(3) as usize;
        let header_x = area.x + 1 + HIGHLIGHT_SYMBOL_WIDTH + col_widths[0] + 1;
        let header_w = (area.x + area.width).saturating_sub(header_x + 1);
        let header_style = theme.header_style();

        for i in 0..visible_rows {
            if let Some(GroupEntry::Header(h)) = rui.table_view.table_entries.get(offset + i) {
                let y = area.y + 2 + i as u16;
                frame.render_widget(Span::styled(format!("── {h} ──"), header_style), Rect::new(header_x, y, header_w, 1));
            }
        }

        // Back up offset if it lands right after a section header
        if offset > 0 && matches!(rui.table_view.table_entries.get(offset - 1), Some(GroupEntry::Header(_))) {
            *rui.table_state.offset_mut() = offset - 1;
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
            .block(Block::bordered().style(theme.block_style()).title_top(Line::from(" ✕ ").right_aligned()));
        frame.render_widget(table, area);
    }
}

impl WorkItemTable {
    // ── Mouse helpers ──

    /// Hit-test a mouse position against the table area to find which
    /// selectable row (if any) was clicked.
    pub(crate) fn row_at_mouse(&self, x: u16, y: u16, ctx: &WidgetContext) -> Option<usize> {
        let ta = self.table_area;
        if x >= ta.x && x < ta.x + ta.width && y >= ta.y && y < ta.y + ta.height {
            let row_in_table = (y - ta.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let repo_key = &ctx.repo_order[ctx.active_repo];
            let rui = &ctx.repo_ui[repo_key];
            let offset = rui.table_state.offset();
            let actual_row = data_row + offset;
            rui.table_view.selectable_indices.iter().position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    // ── Action helpers ──

    fn toggle_providers(ctx: &mut WidgetContext) -> Outcome {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        rui.show_providers = !rui.show_providers;
        Outcome::Consumed
    }
}

impl InteractiveWidget for WorkItemTable {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.select_next(ctx);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.select_prev(ctx);
                Outcome::Consumed
            }
            Action::ToggleMultiSelect => {
                self.toggle_multi_select(ctx);
                Outcome::Consumed
            }
            Action::ToggleProviders => Self::toggle_providers(ctx),
            Action::ToggleHelp => Outcome::Push(Box::new(super::help::HelpWidget::new())),
            Action::OpenBranchInput => Outcome::Push(Box::new(super::branch_input::BranchInputWidget::new(BranchInputKind::Manual))),
            Action::OpenIssueSearch => {
                *ctx.mode = UiMode::IssueSearch { input: tui_input::Input::default() };
                Outcome::Push(Box::new(super::issue_search::IssueSearchWidget::new()))
            }
            Action::OpenCommandPalette => Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new())),
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        let x = mouse.column;
        let y = mouse.row;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Gear icon in the table border area
                if let Some(gear_area) = self.gear_area {
                    if x >= gear_area.x && x < gear_area.x + gear_area.width && y >= gear_area.y && y < gear_area.y + gear_area.height {
                        ctx.app_actions.push(AppAction::ToggleProviders);
                        return Outcome::Consumed;
                    }
                }

                if let Some(si) = self.row_at_mouse(x, y, ctx) {
                    let repo_key = &ctx.repo_order[ctx.active_repo];
                    let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
                    let table_idx = rui.table_view.selectable_indices[si];
                    rui.selected_selectable_idx = Some(si);
                    rui.table_state.select(Some(table_idx));
                    return Outcome::Consumed;
                }

                Outcome::Ignored
            }

            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(si) = self.row_at_mouse(x, y, ctx) {
                    let repo_key = &ctx.repo_order[ctx.active_repo];
                    let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
                    let table_idx = rui.table_view.selectable_indices[si];
                    rui.selected_selectable_idx = Some(si);
                    rui.table_state.select(Some(table_idx));
                    ctx.app_actions.push(AppAction::OpenActionMenu);
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollDown => {
                self.select_next(ctx);
                Outcome::Consumed
            }

            MouseEventKind::ScrollUp => {
                self.select_prev(ctx);
                Outcome::Consumed
            }

            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        // Store area for mouse hit-testing
        self.table_area = area;
        // gear_area is set inside render_table
        self.render_table(ctx.model, ctx.ui, ctx.theme, frame, area);
    }

    fn mode_id(&self) -> ModeId {
        ModeId::Normal
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl Default for WorkItemTable {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helper: active repo UI (immutable borrow) ───────────────────────

fn active_rui<'a>(model: &TuiModel, ui: &'a UiState) -> &'a crate::app::RepoUiState {
    ui.active_repo_ui(&model.repo_order, model.active_repo)
}

// ── Provider table helpers ──────────────────────────────────────────

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

fn build_header_row(_header: &SectionHeader) -> Row<'static> {
    // Empty cells — the actual section title is rendered as an overlay after the
    // table so it can span across all columns regardless of column layout.
    Row::new(vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
    ])
    .height(1)
}

#[allow(clippy::too_many_arguments)]
fn build_item_row<'a>(
    item: &WorkItem,
    providers: &ProviderData,
    col_widths: &[u16],
    repo_root: &Path,
    prev_source: Option<&str>,
    pending: Option<&PendingAction>,
    theme: &Theme,
    home_dir: Option<&Path>,
) -> Row<'a> {
    let session_status = item.session_key.as_deref().and_then(|k| providers.sessions.get(k)).map(|s| &s.status);
    let (icon, icon_color) = ui_helpers::work_item_icon(&item.kind, !item.workspace_refs.is_empty(), session_status, theme);

    let source_display = match item.source.as_deref() {
        Some(s) if prev_source == Some(s) => String::new(),
        Some(s) => s.to_string(),
        None => String::new(),
    };

    let path_width = col_widths.get(2).copied().unwrap_or(14) as usize;
    let desc_width = col_widths.get(3).copied().unwrap_or(15) as usize;
    let branch_width = col_widths.get(4).copied().unwrap_or(25) as usize;

    let path_display = if let Some(p) = item.checkout_key() {
        ui_helpers::shorten_path(&p.path, repo_root, path_width, home_dir)
    } else if let Some(ref ses_key) = item.session_key {
        ses_key.clone()
    } else {
        String::new()
    };
    let path_display = ui_helpers::truncate(&path_display, path_width);

    let description = ui_helpers::truncate(&item.description, desc_width);

    let wt_indicator = ui_helpers::checkout_indicator(item.is_main_checkout, item.checkout_key().is_some());

    let ws_indicator = ui_helpers::workspace_indicator(item.workspace_refs.len());

    let branch = item.branch.as_deref().unwrap_or("—");
    let branch_display = ui_helpers::truncate(branch, branch_width);

    let pr_display = if let Some(ref pr_key) = item.change_request_key {
        if let Some(cr) = providers.change_requests.get(pr_key.as_str()) {
            let state_icon = ui_helpers::change_request_status_icon(&cr.status);
            format!("#{}{}", pr_key, state_icon)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let session_display = if let Some(ref ses_key) = item.session_key {
        if let Some(ses) = providers.sessions.get(ses_key.as_str()) {
            ui_helpers::session_status_display(&ses.status).to_string()
        } else {
            String::new()
        }
    } else if let Some(agent_key) = item.agent_keys.first() {
        if let Some(agent) = providers.agents.get(agent_key.as_str()) {
            ui_helpers::agent_status_display(&agent.status)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let issues_display = item.issue_keys.iter().map(|k| format!("#{}", k)).collect::<Vec<_>>().join(",");

    let git_display = if let Some(wt_key) = item.checkout_key() {
        if let Some(co) = providers.checkouts.get(wt_key) {
            ui_helpers::git_status_display(co)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Pending action rendering — column order must mirror the Row::new path below.
    if let Some(pending) = pending {
        match &pending.status {
            PendingStatus::InFlight => {
                let total_width: usize = col_widths.iter().map(|w| *w as usize).sum();
                let shimmer = Shimmer::new(total_width, theme);
                let spinner = ui_helpers::spinner_char();

                let mut offset: usize = 0;
                let cells = vec![
                    (format!(" {spinner}"), col_widths.first().copied().unwrap_or(3) as usize),
                    (source_display.clone(), col_widths.get(1).copied().unwrap_or(8) as usize),
                    (path_display.clone(), col_widths.get(2).copied().unwrap_or(14) as usize),
                    (description.clone(), col_widths.get(3).copied().unwrap_or(15) as usize),
                    (branch_display.clone(), col_widths.get(4).copied().unwrap_or(25) as usize),
                    (wt_indicator.to_string(), col_widths.get(5).copied().unwrap_or(3) as usize),
                    (ws_indicator.clone(), col_widths.get(6).copied().unwrap_or(3) as usize),
                    (pr_display.clone(), col_widths.get(7).copied().unwrap_or(8) as usize),
                    (session_display.clone(), col_widths.get(8).copied().unwrap_or(8) as usize),
                    (issues_display.clone(), col_widths.get(9).copied().unwrap_or(8) as usize),
                    (git_display.clone(), col_widths.get(10).copied().unwrap_or(5) as usize),
                ];

                let shimmer_cells: Vec<Cell> = cells
                    .into_iter()
                    .map(|(text, width)| {
                        let spans = shimmer.spans(&text, offset);
                        offset += width;
                        Cell::from(Line::from(spans))
                    })
                    .collect();

                return Row::new(shimmer_cells);
            }
            PendingStatus::Failed(_) => {
                let error_style = Style::default().fg(theme.error).add_modifier(Modifier::DIM);
                return Row::new(vec![
                    Cell::from(Span::styled(" \u{2717}", Style::default().fg(theme.error))),
                    Cell::from(Span::styled(source_display, error_style)),
                    Cell::from(Span::styled(path_display, error_style)),
                    Cell::from(Span::styled(description, error_style)),
                    Cell::from(Span::styled(branch_display, error_style)),
                    Cell::from(Span::styled(wt_indicator.to_string(), error_style)),
                    Cell::from(Span::styled(ws_indicator, error_style)),
                    Cell::from(Span::styled(pr_display, error_style)),
                    Cell::from(Span::styled(session_display, error_style)),
                    Cell::from(Span::styled(issues_display, error_style)),
                    Cell::from(Span::styled(git_display, error_style)),
                ]);
            }
        }
    }

    Row::new(vec![
        Cell::from(Span::styled(format!(" {icon}"), Style::default().fg(icon_color))),
        Cell::from(Span::styled(source_display, Style::default().fg(theme.source))),
        Cell::from(Span::styled(path_display, Style::default().fg(theme.path))),
        Cell::from(Span::styled(description, Style::default().fg(theme.text))),
        Cell::from(Span::styled(branch_display, Style::default().fg(theme.branch))),
        Cell::from(Span::styled(wt_indicator.to_string(), Style::default().fg(theme.checkout))),
        Cell::from(Span::styled(ws_indicator, Style::default().fg(theme.workspace))),
        Cell::from(Span::styled(pr_display, Style::default().fg(theme.change_request))),
        Cell::from(Span::styled(session_display, Style::default().fg(theme.session))),
        Cell::from(Span::styled(issues_display, Style::default().fg(theme.issue))),
        Cell::from(Span::styled(git_display, Style::default().fg(theme.git_status))),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{checkout_item, grouped_items, issue_item};

    #[test]
    fn update_items_preserves_selection_by_identity() {
        let mut table = WorkItemTable::new();

        let items_v1 = grouped_items(vec![
            checkout_item("feat/a", "/tmp/a", false),
            checkout_item("feat/b", "/tmp/b", false),
            checkout_item("feat/c", "/tmp/c", false),
        ]);
        table.update_items(items_v1);

        // Select the second item (feat/b)
        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, Some(1));

        // Update with reordered items — feat/b is now at index 2
        let items_v2 = grouped_items(vec![
            checkout_item("feat/a", "/tmp/a", false),
            checkout_item("feat/c", "/tmp/c", false),
            checkout_item("feat/b", "/tmp/b", false),
        ]);
        table.update_items(items_v2);

        // Selection should follow feat/b to its new position
        assert_eq!(table.selected_selectable_idx, Some(2));
        assert_eq!(table.table_state.selected(), Some(2));
    }

    #[test]
    fn update_items_falls_back_to_first_when_selected_removed() {
        let mut table = WorkItemTable::new();

        let items_v1 = grouped_items(vec![checkout_item("feat/a", "/tmp/a", false), checkout_item("feat/b", "/tmp/b", false)]);
        table.update_items(items_v1);

        // Select the second item (feat/b)
        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, Some(1));

        // Update without feat/b
        let items_v2 = grouped_items(vec![checkout_item("feat/a", "/tmp/a", false), checkout_item("feat/c", "/tmp/c", false)]);
        table.update_items(items_v2);

        // Selection should fall back to first item
        assert_eq!(table.selected_selectable_idx, Some(0));
        assert_eq!(table.table_state.selected(), Some(0));
    }

    #[test]
    fn update_items_clears_selection_when_empty() {
        let mut table = WorkItemTable::new();

        let items_v1 = grouped_items(vec![checkout_item("feat/a", "/tmp/a", false)]);
        table.update_items(items_v1);
        assert_eq!(table.selected_selectable_idx, Some(0));

        // Update with empty table
        table.update_items(GroupedWorkItems::default());

        assert_eq!(table.selected_selectable_idx, None);
        assert_eq!(table.table_state.selected(), None);
    }

    #[test]
    fn select_next_self_advances_through_items() {
        let mut table = WorkItemTable::new();
        let items = grouped_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
        table.update_items(items);

        // Starts at 0
        assert_eq!(table.selected_selectable_idx, Some(0));

        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, Some(1));

        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, Some(2));

        // At last item, stays put
        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, Some(2));
    }

    #[test]
    fn select_prev_self_retreats_through_items() {
        let mut table = WorkItemTable::new();
        let items = grouped_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
        table.update_items(items);

        // Move to last
        table.select_next_self();
        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, Some(2));

        table.select_prev_self();
        assert_eq!(table.selected_selectable_idx, Some(1));

        table.select_prev_self();
        assert_eq!(table.selected_selectable_idx, Some(0));

        // At first item, stays put
        table.select_prev_self();
        assert_eq!(table.selected_selectable_idx, Some(0));
    }

    #[test]
    fn select_next_self_noop_on_empty() {
        let mut table = WorkItemTable::new();
        table.select_next_self();
        assert_eq!(table.selected_selectable_idx, None);
    }

    #[test]
    fn select_prev_self_noop_on_empty() {
        let mut table = WorkItemTable::new();
        table.select_prev_self();
        assert_eq!(table.selected_selectable_idx, None);
    }

    #[test]
    fn update_items_selects_first_when_no_prior_selection() {
        let mut table = WorkItemTable::new();
        assert_eq!(table.selected_selectable_idx, None);

        let items = grouped_items(vec![issue_item("1"), issue_item("2")]);
        table.update_items(items);

        assert_eq!(table.selected_selectable_idx, Some(0));
        assert_eq!(table.table_state.selected(), Some(0));
    }
}
