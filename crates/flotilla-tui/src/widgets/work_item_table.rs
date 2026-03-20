use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use flotilla_core::data::{GroupEntry, SectionHeader};
use flotilla_protocol::{HostName, ProviderData, WorkItem};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, HighlightSpacing, Row, Table},
    Frame,
};

use super::WidgetContext;
use crate::{
    app::{
        ui_state::{PendingAction, PendingStatus},
        ProviderStatus, TabId, TuiModel, UiState,
    },
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

/// The work-item table component. Owned by `BaseView` and called via
/// direct methods — it does **not** implement `InteractiveWidget`.
pub struct WorkItemTable;

impl WorkItemTable {
    pub fn new() -> Self {
        Self
    }

    // ── Selection helpers ────────────────────────────────────────────

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

    pub fn render(&self, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        ui.layout.table_area = area;

        let rui = active_rui(model, ui);
        if rui.show_providers {
            let close_x = area.x + area.width.saturating_sub(5);
            ui.layout.tab_areas.insert(TabId::Gear, Rect::new(close_x, area.y, 3, 1));
            self.render_providers(model, ui, theme, frame, area);
            return;
        }

        let gear_x = area.x + area.width.saturating_sub(5);
        ui.layout.tab_areas.insert(TabId::Gear, Rect::new(gear_x, area.y, 3, 1));

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

    pub fn render_providers(&self, model: &TuiModel, _ui: &UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
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
