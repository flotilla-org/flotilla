use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use flotilla_core::data::{GroupEntry, SectionHeader};
use flotilla_protocol::{HostName, ProviderData, WorkItem};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Row, Table},
    Frame,
};

use crate::{
    app::{
        ui_state::{PendingAction, PendingStatus},
        BranchInputKind, InFlightCommand, ProviderStatus, RepoViewLayout, TabId, TuiModel, UiMode, UiState,
    },
    keymap::{Keymap, ModeId},
    shimmer::{shimmer_spans, Shimmer},
    theme::Theme,
    ui_helpers,
};

const HIGHLIGHT_SYMBOL: &str = "▸ ";
const HIGHLIGHT_SYMBOL_WIDTH: u16 = 2;
const PREVIEW_SPLIT_RIGHT_PERCENT: u16 = 40;
const PREVIEW_SPLIT_BELOW_PERCENT: u16 = 40;
const MIN_TABLE_WIDTH: u16 = 50;
const MIN_PREVIEW_WIDTH: u16 = 32;
const MIN_TABLE_HEIGHT: u16 = 8;
const MIN_PREVIEW_HEIGHT: u16 = 6;
const PREVIEW_BELOW_ASPECT_RATIO_THRESHOLD: f32 = 2.0;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedPreviewPosition {
    Right,
    Below,
}

fn resolve_preview_position(area: Rect, layout: RepoViewLayout) -> Option<ResolvedPreviewPosition> {
    match layout {
        RepoViewLayout::Right => Some(ResolvedPreviewPosition::Right),
        RepoViewLayout::Below => Some(ResolvedPreviewPosition::Below),
        RepoViewLayout::Auto => Some(resolve_auto_preview_position(area)),
        RepoViewLayout::Zoom => None,
    }
}

fn resolve_auto_preview_position(area: Rect) -> ResolvedPreviewPosition {
    let right_preview_width = area.width.saturating_mul(PREVIEW_SPLIT_RIGHT_PERCENT) / 100;
    let right_table_width = area.width.saturating_sub(right_preview_width);
    let below_preview_height = area.height.saturating_mul(PREVIEW_SPLIT_BELOW_PERCENT) / 100;
    let below_table_height = area.height.saturating_sub(below_preview_height);

    let right_viable = right_table_width >= MIN_TABLE_WIDTH && right_preview_width >= MIN_PREVIEW_WIDTH;
    let below_viable = below_table_height >= MIN_TABLE_HEIGHT && below_preview_height >= MIN_PREVIEW_HEIGHT;

    match (right_viable, below_viable) {
        (true, false) => ResolvedPreviewPosition::Right,
        (false, true) => ResolvedPreviewPosition::Below,
        (false, false) => ResolvedPreviewPosition::Right,
        (true, true) => {
            let aspect_ratio = area.width as f32 / area.height as f32;
            if aspect_ratio < PREVIEW_BELOW_ASPECT_RATIO_THRESHOLD {
                ResolvedPreviewPosition::Below
            } else {
                ResolvedPreviewPosition::Right
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    model: &TuiModel,
    ui: &mut UiState,
    in_flight: &HashMap<u64, InFlightCommand>,
    theme: &Theme,
    _keymap: &Keymap,
    frame: &mut Frame,
    active_widget_mode: Option<ModeId>,
    tab_bar: &mut crate::widgets::tab_bar::TabBar,
    status_bar_widget: &mut crate::widgets::status_bar_widget::StatusBarWidget,
    event_log_widget: &mut crate::widgets::event_log::EventLogWidget,
    preview_panel: &crate::widgets::preview_panel::PreviewPanel,
) {
    let constraints = vec![Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)];
    let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(frame.area());

    tab_bar.render(model, ui, theme, frame, chunks[0]);
    render_content(model, ui, theme, frame, chunks[1], event_log_widget, preview_panel);

    // Write event log filter area to shared layout for click detection
    ui.layout.event_log_filter_area = event_log_widget.filter_area();

    // When the palette is active, move the status bar to the top of the overlay so the
    // input sits above the results instead of being pinned to the bottom of the screen.
    let status_bar_area = if matches!(ui.mode, UiMode::CommandPalette { .. }) {
        ui_helpers::bottom_anchored_overlay(frame.area(), 1, crate::palette::MAX_PALETTE_ROWS as u16).status_row
    } else {
        chunks[2]
    };
    status_bar_widget.render(model, ui, in_flight, theme, frame, status_bar_area, active_widget_mode);
    render_command_palette(ui, theme, frame, status_bar_area);
    render_input_popup(ui, theme, frame);
    render_file_picker(ui, theme, frame);
}

fn active_rui<'a>(model: &TuiModel, ui: &'a UiState) -> &'a crate::app::RepoUiState {
    ui.active_repo_ui(&model.repo_order, model.active_repo)
}

// ── Provider table helpers (shared with render_repo_providers) ────────

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

fn render_content(
    model: &TuiModel,
    ui: &mut UiState,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
    event_log_widget: &mut crate::widgets::event_log::EventLogWidget,
    preview_panel: &crate::widgets::preview_panel::PreviewPanel,
) {
    if ui.mode.is_config() {
        event_log_widget.render_config_screen(model, theme, frame, area);
        return;
    }

    let Some(position) = resolve_preview_position(area, ui.view_layout) else {
        render_unified_table(model, ui, theme, frame, area);
        return;
    };

    let chunks = match position {
        ResolvedPreviewPosition::Right => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100 - PREVIEW_SPLIT_RIGHT_PERCENT), Constraint::Percentage(PREVIEW_SPLIT_RIGHT_PERCENT)])
            .split(area),
        ResolvedPreviewPosition::Below => Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100 - PREVIEW_SPLIT_BELOW_PERCENT), Constraint::Percentage(PREVIEW_SPLIT_BELOW_PERCENT)])
            .split(area),
    };

    render_unified_table(model, ui, theme, frame, chunks[0]);
    preview_panel.render(model, ui, theme, frame, chunks[1]);
}

fn render_repo_providers(model: &TuiModel, _ui: &UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
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

fn render_unified_table(model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    ui.layout.table_area = area;

    let rui = active_rui(model, ui);
    if rui.show_providers {
        let close_x = area.x + area.width.saturating_sub(5);
        ui.layout.tab_areas.insert(TabId::Gear, Rect::new(close_x, area.y, 3, 1));
        render_repo_providers(model, ui, theme, frame, area);
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
                    // Look up home_dir from the checkout's host summary. Only fall
                    // back to dirs::home_dir() for local-host items — using the local
                    // home for a remote host would incorrectly shorten unrelated paths.
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

    // Overlay section headers so they span the full row width, independent of
    // column layout.  The Table rendered empty cells for header rows; we draw
    // the title text on top, starting from after the icon column.
    let offset = rui.table_state.offset();
    let visible_rows = area.height.saturating_sub(3) as usize; // borders + column header
    let header_x = area.x + 1 + HIGHLIGHT_SYMBOL_WIDTH + col_widths[0] + 1; // border + highlight + icon + spacing
    let header_w = (area.x + area.width).saturating_sub(header_x + 1); // up to right border
    let header_style = theme.header_style();

    for i in 0..visible_rows {
        if let Some(GroupEntry::Header(h)) = rui.table_view.table_entries.get(offset + i) {
            let y = area.y + 2 + i as u16;
            frame.render_widget(Span::styled(format!("── {h} ──"), header_style), Rect::new(header_x, y, header_w, 1));
        }
    }

    // Ratatui scrolls just enough to show the selected row, but section headers
    // sit one row above the first item in each section.  If the offset lands
    // right after a header, back it up so the header stays visible.
    if offset > 0 && matches!(rui.table_view.table_entries.get(offset - 1), Some(GroupEntry::Header(_))) {
        *rui.table_state.offset_mut() = offset - 1;
    }
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

fn render_input_popup(ui: &UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::BranchInput { ref input, ref kind, .. } = ui.mode else {
        return;
    };

    let (_area, inner_area) = ui_helpers::render_popup_frame(frame, frame.area(), 50, 20, " New Branch ", theme.block_style());

    if *kind == BranchInputKind::Generating {
        let spans = shimmer_spans("  Generating branch name...", theme);
        let paragraph = Paragraph::new(Line::from(spans));
        frame.render_widget(paragraph, inner_area);
        return;
    }

    let input_text = input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(theme.input_text));
    frame.render_widget(paragraph, inner_area);

    let cursor_x = inner_area.x + 2 + input.visual_cursor() as u16;
    let cursor_y = inner_area.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_file_picker(ui: &mut UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::FilePicker { ref input, ref dir_entries, selected } = ui.mode else {
        return;
    };

    let (area, inner) = ui_helpers::render_popup_frame(frame, frame.area(), 60, 60, " Add Repository ", theme.block_style());
    ui.layout.file_picker_area = area;

    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(inner);

    ui.layout.file_picker_list_area = chunks[1];

    let input_text = input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(theme.input_text));
    frame.render_widget(paragraph, chunks[0]);

    let cursor_x = chunks[0].x + 2 + input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, chunks[0].y));

    let items: Vec<ListItem> = dir_entries
        .iter()
        .map(|entry| {
            let tag = if entry.is_added {
                " (added)"
            } else if entry.is_git_repo {
                " (git repo)"
            } else if entry.is_dir {
                "/"
            } else {
                ""
            };
            let style = if entry.is_git_repo && !entry.is_added {
                Style::default().fg(theme.status_ok)
            } else if entry.is_added {
                Style::default().fg(theme.muted)
            } else {
                Style::default()
            };
            ListItem::new(format!("  {}{}", entry.name, tag)).style(style)
        })
        .collect();

    let list = List::new(items).highlight_style(Style::default().bg(theme.row_highlight).bold()).highlight_symbol("▸ ");

    let mut state = ListState::default();
    if !dir_entries.is_empty() {
        state.select(Some(selected));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn render_command_palette(ui: &UiState, theme: &Theme, frame: &mut Frame, status_bar_area: Rect) {
    let UiMode::CommandPalette { ref input, entries, selected, scroll_top } = ui.mode else {
        return;
    };

    let filtered: Vec<&crate::palette::PaletteEntry> = crate::palette::filter_entries(entries, input.value());
    let overlay = ui_helpers::bottom_anchored_overlay(frame.area(), 1, crate::palette::MAX_PALETTE_ROWS as u16);
    let area = overlay.body;

    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

    let name_width = filtered.iter().map(|e| e.name.len()).max().unwrap_or(0).min(20);
    let hint_width: u16 = 7;

    for (i, entry) in filtered.iter().skip(scroll_top).take(overlay.visible_body_rows as usize).enumerate() {
        let row_y = area.y + i as u16;
        let is_selected = scroll_top + i == selected;

        let row_style = if is_selected {
            Style::default().bg(theme.action_highlight).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(theme.bar_bg)
        };

        let row_area = Rect::new(area.x, row_y, area.width, 1);
        frame.render_widget(Block::default().style(row_style), row_area);

        let name_span = Span::styled(format!("  {:<width$}", entry.name, width = name_width), row_style.fg(theme.text));
        let desc_span = Span::styled(format!("  {}", entry.description), row_style.fg(theme.muted));

        let line = Line::from(vec![name_span, desc_span]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x, row_y, area.width.saturating_sub(hint_width), 1));

        let hint_text = entry.key_hint.unwrap_or("");
        if !hint_text.is_empty() {
            let hint_span = Span::styled(format!(" {} ", hint_text), row_style.fg(theme.key_hint));
            let hint_x = area.x + area.width.saturating_sub(hint_width);
            frame.render_widget(Paragraph::new(Line::from(hint_span)), Rect::new(hint_x, row_y, hint_width, 1));
        }
    }

    // Cursor on the status bar row
    let cursor_x = status_bar_area.x + 1 + input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, status_bar_area.y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::RepoViewLayout;

    #[test]
    fn auto_layout_prefers_right_when_wide() {
        let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Auto);
        assert_eq!(position, Some(ResolvedPreviewPosition::Right));
    }

    #[test]
    fn auto_layout_prefers_below_when_tall() {
        let position = resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Auto);
        assert_eq!(position, Some(ResolvedPreviewPosition::Below));
    }

    #[test]
    fn explicit_right_layout() {
        let position = resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Right);
        assert_eq!(position, Some(ResolvedPreviewPosition::Right));
    }

    #[test]
    fn explicit_below_layout() {
        let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Below);
        assert_eq!(position, Some(ResolvedPreviewPosition::Below));
    }

    #[test]
    fn zoom_layout_returns_none() {
        let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Zoom);
        assert_eq!(position, None);
    }

    #[test]
    fn auto_neither_viable_falls_back_to_right() {
        // 60x10: right_preview_width = 24 (< MIN_PREVIEW_WIDTH 32),
        //        below_preview_height = 4 (< MIN_PREVIEW_HEIGHT 6)
        // Both layouts are non-viable, so fallback to Right.
        let result = resolve_auto_preview_position(Rect::new(0, 0, 60, 10));
        assert_eq!(result, ResolvedPreviewPosition::Right);
    }

    #[test]
    fn auto_only_right_viable() {
        // 210x10: right_preview_width = 84 (>= 32), right_table_width = 126 (>= 50) → viable
        //         below_preview_height = 4 (< 6) → not viable
        let result = resolve_auto_preview_position(Rect::new(0, 0, 210, 10));
        assert_eq!(result, ResolvedPreviewPosition::Right);
    }

    #[test]
    fn auto_only_below_viable() {
        // 60x40: right_preview_width = 24 (< 32) → not viable
        //        below_preview_height = 16 (>= 6), below_table_height = 24 (>= 8) → viable
        let result = resolve_auto_preview_position(Rect::new(0, 0, 60, 40));
        assert_eq!(result, ResolvedPreviewPosition::Below);
    }

    #[test]
    fn auto_both_viable_wide_prefers_right() {
        // 160x40: both viable, aspect_ratio = 4.0 (>= 2.0) → Right
        let result = resolve_auto_preview_position(Rect::new(0, 0, 160, 40));
        assert_eq!(result, ResolvedPreviewPosition::Right);
    }

    #[test]
    fn auto_both_viable_tall_prefers_below() {
        // 90x50: both viable, aspect_ratio = 1.8 (< 2.0) → Below
        let result = resolve_auto_preview_position(Rect::new(0, 0, 90, 50));
        assert_eq!(result, ResolvedPreviewPosition::Below);
    }
}
