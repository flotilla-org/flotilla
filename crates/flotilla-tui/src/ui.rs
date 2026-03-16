use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crossterm::event::KeyCode;
use flotilla_core::data::{GroupEntry, SectionHeader};
use flotilla_protocol::{HostName, ProviderData, WorkItem};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Row, Table, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{
        collect_visible_status_items,
        ui_state::{PendingAction, PendingStatus},
        BranchInputKind, InFlightCommand, PeerStatus, ProviderStatus, RepoViewLayout, TabId, TuiHostState, TuiModel, UiMode, UiState,
    },
    event_log::{self, LevelExt},
    keymap::Keymap,
    segment_bar::{self, BarStyle, ThemedRibbonStyle, ThemedTabBarStyle},
    shimmer::{shimmer_spans, Shimmer},
    status_bar::{
        KeyChip, StatusBarAction, StatusBarInput, StatusBarModel, StatusBarTarget, StatusSection, TaskSection, DEFAULT_STATUS_WIDTH_BUDGET,
    },
    theme::{BarKind, Theme},
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
const ENTER_KEY_GLYPH: &str = "ENT";

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

pub fn render(
    model: &TuiModel,
    ui: &mut UiState,
    in_flight: &HashMap<u64, InFlightCommand>,
    theme: &Theme,
    keymap: &Keymap,
    frame: &mut Frame,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    render_tab_bar(model, ui, theme, frame, chunks[0]);
    render_content(model, ui, theme, frame, chunks[1]);
    render_status_bar(model, ui, in_flight, theme, frame, chunks[2]);
    render_action_menu(model, ui, theme, frame);
    render_input_popup(ui, theme, frame);
    render_delete_confirm(model, ui, theme, frame);
    render_close_confirm(model, ui, theme, frame);
    render_help(ui, theme, keymap, frame);
    render_file_picker(ui, theme, frame);
}

fn render_tab_bar(model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let mut items = Vec::new();
    let mut tab_ids = Vec::new();

    // Flotilla logo tab
    let flotilla_style = theme.logo_style(ui.mode.is_config());
    items.push(segment_bar::SegmentItem {
        label: TabId::FLOTILLA_LABEL.to_string(),
        key_hint: None,
        active: ui.mode.is_config(),
        dragging: false,
        style_override: Some(flotilla_style),
    });
    tab_ids.push(TabId::Flotilla);

    // Repo tabs
    for (i, repo_identity) in model.repo_order.iter().enumerate() {
        let rm = &model.repos[repo_identity];
        let rui = &ui.repo_ui[repo_identity];
        let name = TuiModel::repo_name(&rm.path);
        let is_active = !ui.mode.is_config() && i == model.active_repo;
        let loading = if rm.loading { " ⟳" } else { "" };
        let changed = if rui.has_unseen_changes { "*" } else { "" };
        let label = format!("{name}{changed}{loading}");

        items.push(segment_bar::SegmentItem {
            label,
            key_hint: None,
            active: is_active,
            dragging: is_active && ui.drag.active,
            style_override: None,
        });
        tab_ids.push(TabId::Repo(i));
    }

    // [+] button
    items.push(segment_bar::SegmentItem {
        label: "[+]".to_string(),
        key_hint: None,
        active: false,
        dragging: false,
        style_override: Some(Style::default().fg(theme.status_ok)),
    });
    tab_ids.push(TabId::Add);

    // Render
    let tab_style: Box<dyn BarStyle> = match theme.tab_bar.kind {
        BarKind::Pipe => Box::new(ThemedTabBarStyle { theme, site: &theme.tab_bar }),
        BarKind::Chevron => Box::new(ThemedRibbonStyle { theme, site: &theme.tab_bar }),
    };
    let hits = segment_bar::render(&items, tab_style.as_ref(), area, frame.buffer_mut());

    // Map hit regions to tab areas
    ui.layout.tab_areas.clear();
    for hit in hits {
        if let Some(tab_id) = tab_ids.get(hit.index) {
            ui.layout.tab_areas.insert(tab_id.clone(), hit.area);
        }
    }
}

fn active_rui<'a>(model: &TuiModel, ui: &'a UiState) -> &'a crate::app::RepoUiState {
    ui.active_repo_ui(&model.repo_order, model.active_repo)
}

fn provider_status_badge(status: Option<ProviderStatus>, theme: &Theme) -> (&'static str, Color) {
    match status {
        Some(ProviderStatus::Ok) => ("✓", theme.status_ok),
        Some(ProviderStatus::Error) => ("✗", theme.error),
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
        Cell::from(Span::styled("—", Style::default().fg(theme.muted))),
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

fn selected_work_item<'a>(model: &TuiModel, ui: &'a UiState) -> Option<&'a WorkItem> {
    let rui = active_rui(model, ui);
    let table_idx = rui.table_state.selected()?;
    match rui.table_view.table_entries.get(table_idx)? {
        GroupEntry::Item(item) => Some(item),
        GroupEntry::Header(_) => None,
    }
}

fn render_status_bar(
    model: &TuiModel,
    ui: &mut UiState,
    in_flight: &HashMap<u64, InFlightCommand>,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    ui.layout.status_bar.area = area;
    ui.layout.status_bar.key_targets.clear();
    ui.layout.status_bar.dismiss_targets.clear();

    let (status_section, keys, task_section) = status_bar_content(model, ui, in_flight);
    let status_model = StatusBarModel::build(StatusBarInput {
        width: area.width as usize,
        preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET.min(area.width as usize),
        keys_visible: ui.status_bar.show_keys,
        status: status_section.clone(),
        task: task_section,
        keys,
    });

    frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

    let mut spans = Vec::new();
    let mut x = 0usize;
    let status_style = match status_section {
        StatusSection::Error { .. } => Style::default().fg(theme.status_error).bg(theme.bar_bg).bold(),
        StatusSection::Plain(_) => Style::default().fg(theme.text).bg(theme.bar_bg),
    };

    if !status_model.status_text.is_empty() {
        let status_width = status_model.status_text.width();
        spans.push(Span::styled(status_model.status_text.clone(), status_style));
        if let Some(id) = status_section.dismiss_id() {
            ui.layout.status_bar.dismiss_targets.push(StatusBarTarget::new(
                Rect::new(area.x + status_width.saturating_sub(1) as u16, area.y, 1, 1),
                StatusBarAction::ClearError(id),
            ));
        } else if matches!(ui.mode, UiMode::Normal) && status_model.status_text == layout_status_text(ui) {
            ui.layout
                .status_bar
                .key_targets
                .push(StatusBarTarget::new(Rect::new(area.x, area.y, status_width as u16, 1), StatusBarAction::key(KeyCode::Char('l'))));
        }
        x += status_width;
    }

    if x < status_model.keys_start {
        spans.push(Span::styled(" ".repeat(status_model.keys_start - x), Style::default().fg(theme.text).bg(theme.bar_bg)));
        x = status_model.keys_start;
    }

    let ribbon_style = ThemedRibbonStyle { theme, site: &theme.status_bar };
    for chip in &status_model.visible_keys {
        let ribbon_start = x;
        let item = segment_bar::SegmentItem {
            label: chip.label.clone(),
            key_hint: Some(chip.key.clone()),
            active: false,
            dragging: false,
            style_override: None,
        };
        let rendered = ribbon_style.render_item(&item);
        for span in rendered.spans {
            spans.push(span);
        }

        ui.layout
            .status_bar
            .key_targets
            .push(StatusBarTarget::new(Rect::new(area.x + ribbon_start as u16, area.y, rendered.width as u16, 1), chip.action.clone()));
        x += rendered.width;
    }

    if x < status_model.task_start {
        spans.push(Span::styled(" ".repeat(status_model.task_start - x), Style::default().fg(theme.text).bg(theme.bar_bg)));
        x = status_model.task_start;
    }

    if !status_model.task_text.is_empty() {
        let task_spans = shimmer_spans(&status_model.task_text, theme);
        for mut s in task_spans {
            s.style = s.style.bg(theme.bar_bg);
            spans.push(s);
        }
        x += status_model.task_text.width();
    }

    if x < area.width as usize {
        spans.push(Span::styled(" ".repeat(area.width as usize - x), Style::default().fg(theme.text).bg(theme.bar_bg)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn status_bar_content(
    model: &TuiModel,
    ui: &UiState,
    in_flight: &HashMap<u64, InFlightCommand>,
) -> (StatusSection, Vec<KeyChip>, Option<TaskSection>) {
    let visible_error = collect_visible_status_items(model, ui).into_iter().next();

    match &ui.mode {
        UiMode::Normal => {
            let rui = active_rui(model, ui);
            let status = if let Some(item) = visible_error {
                StatusSection::error(item.id, &item.text)
            } else if rui.show_providers {
                StatusSection::plain("PROVIDERS")
            } else if let Some(query) = rui.active_search_query.as_deref() {
                StatusSection::plain(&format!("SEARCH \"{query}\""))
            } else if !rui.multi_selected.is_empty() {
                StatusSection::plain(&format!("{} SELECTED", rui.multi_selected.len()))
            } else {
                StatusSection::plain(layout_status_text(ui))
            };

            let task = active_task(model, in_flight).map(|(description, spinner_index)| TaskSection::new(&description, spinner_index));
            (status, normal_mode_key_chips(ui), task)
        }
        UiMode::Config => (
            StatusSection::plain("FLOTILLA"),
            vec![
                key_chip("j", "Down", KeyCode::Char('j')),
                key_chip("k", "Up", KeyCode::Char('k')),
                key_chip("[", "Prev", KeyCode::Char('[')),
                key_chip("]", "Next", KeyCode::Char(']')),
                key_chip("q", "Quit", KeyCode::Char('q')),
            ],
            None,
        ),
        UiMode::BranchInput { kind: BranchInputKind::Generating, .. } => {
            (StatusSection::plain("NEW BRANCH"), vec![], Some(TaskSection::new("Generating branch name...", 0)))
        }
        UiMode::BranchInput { kind: BranchInputKind::Manual, .. } => (
            StatusSection::plain("NEW BRANCH"),
            vec![key_chip(ENTER_KEY_GLYPH, "Create", KeyCode::Enter), key_chip("esc", "Cancel", KeyCode::Esc)],
            None,
        ),
        UiMode::ActionMenu { .. } => (
            StatusSection::plain("ACTIONS"),
            vec![
                key_chip("j", "Down", KeyCode::Char('j')),
                key_chip("k", "Up", KeyCode::Char('k')),
                key_chip(ENTER_KEY_GLYPH, "Select", KeyCode::Enter),
                key_chip("esc", "Close", KeyCode::Esc),
            ],
            None,
        ),
        UiMode::IssueSearch { input } => (
            StatusSection::plain(&format!("SEARCH {}", input.value())),
            vec![key_chip(ENTER_KEY_GLYPH, "Apply", KeyCode::Enter), key_chip("esc", "Cancel", KeyCode::Esc)],
            None,
        ),
        UiMode::FilePicker { .. } => (
            StatusSection::plain("ADD REPO"),
            vec![
                key_chip("j", "Down", KeyCode::Char('j')),
                key_chip("k", "Up", KeyCode::Char('k')),
                key_chip("tab", "Complete", KeyCode::Tab),
                key_chip(ENTER_KEY_GLYPH, "Select", KeyCode::Enter),
                key_chip("esc", "Cancel", KeyCode::Esc),
            ],
            None,
        ),
        UiMode::DeleteConfirm { .. } => (
            StatusSection::plain("CONFIRM DELETE"),
            vec![key_chip("y", "Yes", KeyCode::Char('y')), key_chip("n", "No", KeyCode::Char('n'))],
            None,
        ),
        UiMode::CloseConfirm { .. } => (
            StatusSection::plain("CONFIRM CLOSE"),
            vec![key_chip("y", "Yes", KeyCode::Char('y')), key_chip("n", "No", KeyCode::Char('n'))],
            None,
        ),
        UiMode::Help => (
            StatusSection::plain("HELP"),
            vec![
                key_chip("j", "Down", KeyCode::Char('j')),
                key_chip("k", "Up", KeyCode::Char('k')),
                key_chip("esc", "Close", KeyCode::Esc),
                key_chip("?", "Close", KeyCode::Char('?')),
            ],
            None,
        ),
    }
}

fn active_task(model: &TuiModel, in_flight: &HashMap<u64, InFlightCommand>) -> Option<(String, usize)> {
    let active_repo = &model.repo_order[model.active_repo];
    let active_cmds: Vec<&str> =
        in_flight.values().filter(|cmd| &cmd.repo_identity == active_repo).map(|cmd| cmd.description.as_str()).collect();

    if active_cmds.is_empty() {
        return None;
    }

    let description =
        if active_cmds.len() == 1 { active_cmds[0].to_string() } else { format!("{} (+{})", active_cmds[0], active_cmds.len() - 1) };

    Some((description, 0))
}

fn normal_mode_key_chips(ui: &UiState) -> Vec<KeyChip> {
    vec![
        key_chip(ENTER_KEY_GLYPH, "Open", KeyCode::Enter),
        key_chip(".", "Menu", KeyCode::Char('.')),
        key_chip("/", "Search", KeyCode::Char('/')),
        key_chip("h", &target_host_key_label(ui), KeyCode::Char('h')),
        key_chip("n", "New", KeyCode::Char('n')),
        key_chip("?", "Help", KeyCode::Char('?')),
        key_chip("q", "Quit", KeyCode::Char('q')),
    ]
}

fn target_host_key_label(ui: &UiState) -> String {
    match ui.target_host.as_ref() {
        Some(host) => format!("Host {host}"),
        None => "Host Local".into(),
    }
}

fn key_chip(key: &str, label: &str, code: KeyCode) -> KeyChip {
    KeyChip::new(key, label, StatusBarAction::key(code))
}

fn render_content(model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    if ui.mode.is_config() {
        render_config_screen(model, ui, theme, frame, area);
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
    render_preview(model, ui, theme, frame, chunks[1]);
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
        Cell::from(description),
        Cell::from(Span::styled(branch_display, Style::default().fg(theme.branch))),
        Cell::from(Span::styled(wt_indicator.to_string(), Style::default().fg(theme.checkout))),
        Cell::from(Span::styled(ws_indicator, Style::default().fg(theme.workspace))),
        Cell::from(Span::styled(pr_display, Style::default().fg(theme.change_request))),
        Cell::from(Span::styled(session_display, Style::default().fg(theme.session))),
        Cell::from(Span::styled(issues_display, Style::default().fg(theme.issue))),
        Cell::from(Span::styled(git_display, Style::default().fg(theme.git_status))),
    ])
}

fn render_preview(model: &TuiModel, ui: &UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    if ui.show_debug {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        render_preview_content(model, ui, theme, frame, chunks[0]);
        render_debug_panel(model, ui, theme, frame, chunks[1]);
    } else {
        render_preview_content(model, ui, theme, frame, area);
    }
}

fn render_preview_content(model: &TuiModel, ui: &UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let text = if let Some(item) = selected_work_item(model, ui) {
        let rm = model.active();
        let providers = &rm.providers;
        let mut lines = Vec::new();

        lines.push(format!("Description: {}", item.description));

        if let Some(ref branch) = item.branch {
            lines.push(format!("Branch: {}", branch));
        }

        if let Some(wt_key) = item.checkout_key() {
            if let Some(co) = providers.checkouts.get(wt_key) {
                lines.push(format!("Path: {}", wt_key.path.display()));
                if let Some(commit) = &co.last_commit {
                    let sha = if commit.short_sha.is_empty() { "?" } else { &commit.short_sha };
                    lines.push(format!("Commit: {} {}", sha, commit.message));
                }
                if let Some(main) = &co.trunk_ahead_behind {
                    if main.ahead > 0 || main.behind > 0 {
                        lines.push(format!("vs main: +{} -{}", main.ahead, main.behind));
                    }
                }
                if let Some(remote) = &co.remote_ahead_behind {
                    if remote.ahead > 0 || remote.behind > 0 {
                        lines.push(format!("vs remote: +{} -{}", remote.ahead, remote.behind));
                    }
                }
            }
        }

        if let Some(ref pr_key) = item.change_request_key {
            if let Some(cr) = providers.change_requests.get(pr_key.as_str()) {
                let provider_prefix =
                    if cr.provider_display_name.is_empty() { String::new() } else { format!("{} ", cr.provider_display_name) };
                lines.push(format!("{}{} #{}: {}", provider_prefix, model.active_labels().change_requests.abbr, pr_key, cr.title));
                lines.push(format!("State: {:?}", cr.status));
            }
        }

        if let Some(ref ses_key) = item.session_key {
            if let Some(ses) = providers.sessions.get(ses_key.as_str()) {
                let noun =
                    if ses.item_noun.is_empty() { model.active_labels().cloud_agents.noun_capitalized() } else { ses.item_noun.clone() };
                let provider_prefix =
                    if ses.provider_display_name.is_empty() { noun } else { format!("{} {}", ses.provider_display_name, noun) };
                lines.push(format!("{}: {}", provider_prefix, ses.title));
                lines.push(format!("Id: {}", ses_key));
                lines.push(format!("Status: {:?}", ses.status));
                if let Some(ref model_name) = ses.model {
                    lines.push(format!("Model: {}", model_name));
                }
                if let Some(ref updated) = ses.updated_at {
                    let display = updated.split('T').next().unwrap_or(updated);
                    lines.push(format!("Updated: {}", display));
                }
            }
        }

        for ws_ref in &item.workspace_refs {
            if let Some(ws) = providers.workspaces.get(ws_ref.as_str()) {
                let name = if ws.name.is_empty() { ws_ref.as_str() } else { &ws.name };
                lines.push(format!("Workspace: {}", name));
            }
        }

        for issue_key in &item.issue_keys {
            if let Some(issue) = providers.issues.get(issue_key.as_str()) {
                let labels = issue.labels.join(", ");
                let provider_prefix =
                    if issue.provider_display_name.is_empty() { String::new() } else { format!("{} ", issue.provider_display_name) };
                lines.push(format!("{}Issue #{}: {} [{}]", provider_prefix, issue_key, issue.title, labels));
            }
        }

        lines.join("\n")
    } else {
        String::new()
    };

    let preview = Paragraph::new(text).block(Block::bordered().style(theme.block_style()).title(" Preview ")).wrap(Wrap { trim: true });
    frame.render_widget(preview, area);
}

fn render_debug_panel(model: &TuiModel, ui: &UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let text = if let Some(item) = selected_work_item(model, ui) {
        if !item.debug_group.is_empty() {
            item.debug_group.join("\n")
        } else {
            "Not correlated (standalone)".into()
        }
    } else {
        String::new()
    };

    let panel =
        Paragraph::new(text).block(Block::bordered().style(theme.block_style()).title(" Debug (D to toggle) ")).wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn render_action_menu(model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::ActionMenu { ref items, index } = ui.mode else {
        return;
    };

    let area = ui_helpers::popup_area(frame.area(), 40, 40);
    ui.layout.menu_area = area;
    frame.render_widget(Clear, area);

    let labels = model.active_labels();
    let list_items: Vec<ListItem> =
        items.iter().enumerate().map(|(i, intent)| ListItem::new(format!(" {}: {}", i + 1, intent.label(labels)))).collect();

    let list = List::new(list_items)
        .block(Block::bordered().style(theme.block_style()).title(" Actions "))
        .highlight_style(Style::default().bg(theme.action_highlight).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    state.select(Some(index));
    frame.render_stateful_widget(list, area, &mut state);
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

fn render_delete_confirm(model: &TuiModel, ui: &UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::DeleteConfirm { ref info, loading, .. } = ui.mode else {
        return;
    };

    let area = ui_helpers::popup_area(frame.area(), 60, 50);
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Wrap { trim: true } strips leading whitespace, so don't add prefix spaces.
    const MAX_FILES: usize = 10;
    const MAX_COMMITS: usize = 5;

    if loading {
        lines.push(Line::from(shimmer_spans("Loading safety info...", theme)));
    } else if let Some(info) = info {
        lines.push(Line::from(vec![Span::raw("Branch: "), Span::styled(&info.branch, Style::default().bold())]));
        lines.push(Line::from(""));

        if let Some(pr_status) = &info.change_request_status {
            let color = theme.change_request_status_color(pr_status);
            let status_text = pr_status.as_str();
            lines.push(Line::from(vec![
                Span::raw(format!("{}: ", model.active_labels().change_requests.abbr)),
                Span::styled(status_text, Style::default().fg(color).bold()),
            ]));
            if let Some(sha) = &info.merge_commit_sha {
                lines.push(Line::from(format!("Merge commit: {}", sha)));
            }
        } else {
            lines.push(Line::from(Span::styled(
                format!("No {} found", model.active_labels().change_requests.abbr),
                Style::default().fg(theme.muted),
            )));
        }

        lines.push(Line::from(""));

        if info.has_uncommitted {
            if info.uncommitted_files.is_empty() {
                lines.push(Line::from(Span::styled("⚠ Has uncommitted changes", Style::default().fg(theme.error).bold())));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("⚠ {} uncommitted file(s):", info.uncommitted_files.len()),
                    Style::default().fg(theme.error).bold(),
                )));
                for file_line in info.uncommitted_files.iter().take(MAX_FILES) {
                    lines.push(Line::from(Span::styled(file_line.to_string(), Style::default().fg(theme.muted))));
                }
                if info.uncommitted_files.len() > MAX_FILES {
                    lines.push(Line::from(Span::styled(
                        format!("...and {} more", info.uncommitted_files.len() - MAX_FILES),
                        Style::default().fg(theme.muted),
                    )));
                }
            }
        }

        if let Some(warning) = &info.base_detection_warning {
            lines.push(Line::from(Span::styled(format!("⚠ {}", warning), Style::default().fg(theme.warning))));
        } else if !info.unpushed_commits.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("⚠ {} unpushed commit(s):", info.unpushed_commits.len()),
                Style::default().fg(theme.error).bold(),
            )));
            for commit in info.unpushed_commits.iter().take(MAX_COMMITS) {
                lines.push(Line::from(commit.to_string()));
            }
        }

        if !info.has_uncommitted
            && info.unpushed_commits.is_empty()
            && info.base_detection_warning.is_none()
            && info.change_request_status.as_ref().is_some_and(|s| s.eq_ignore_ascii_case("merged"))
        {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("✓ Safe to delete", Style::default().fg(theme.status_ok).bold())));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("y/Enter: confirm    n/Esc: cancel", Style::default().fg(theme.muted))));
    }

    let title = format!(" Remove {} ", model.active_labels().checkouts.noun_capitalized());
    let paragraph = Paragraph::new(lines).block(Block::bordered().style(theme.block_style()).title(title)).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_close_confirm(model: &TuiModel, ui: &UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::CloseConfirm { ref id, ref title, .. } = ui.mode else {
        return;
    };

    let area = ui_helpers::popup_area(frame.area(), 50, 30);
    frame.render_widget(Clear, area);

    let noun = &model.active_labels().change_requests.noun;
    let lines = vec![
        Line::from(vec![Span::raw(format!("{} #", noun)), Span::styled(id, Style::default().bold())]),
        Line::from(Span::styled(title.as_str(), Style::default().fg(theme.muted))),
        Line::from(""),
        Line::from(Span::styled("y/Enter: confirm    n/Esc: cancel", Style::default().fg(theme.muted))),
    ];

    let block_title = format!(" Close {} ", noun);
    let paragraph = Paragraph::new(lines).block(Block::bordered().style(theme.block_style()).title(block_title)).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_help(ui: &mut UiState, theme: &Theme, keymap: &Keymap, frame: &mut Frame) {
    if !matches!(ui.mode, UiMode::Help) {
        return;
    }

    let area = ui_helpers::popup_area(frame.area(), 60, 85);
    frame.render_widget(Clear, area);

    let mut help_text = vec![
        Line::from(Span::styled("Item Icons", Style::default().bold())),
        Line::from("  ●  Checkout with workspace    ○  Checkout (no workspace)"),
        Line::from("  ▶  Running session            ◆  Idle session"),
        Line::from("  ⊙  Pull request               ◇  Issue"),
        Line::from("  ⊶  Remote branch"),
        Line::from(""),
        Line::from(Span::styled("Column Indicators", Style::default().bold())),
        Line::from("  WT: ◆ main  ✓ checked out"),
        Line::from("  WS: ● has workspace  2/3/… multiple"),
        Line::from("  PR: ✓ merged  ✗ closed"),
        Line::from("  Git: ? untracked  M modified  ↑ ahead  ↓ behind"),
        Line::from(""),
    ];

    // Dynamic sections from keymap
    for section in keymap.help_sections() {
        help_text.push(Line::from(Span::styled(section.title, Style::default().bold())));
        for binding in &section.bindings {
            help_text.push(Line::from(format!("  {:18}{}", binding.key_display, binding.description)));
        }
        help_text.push(Line::from(""));
    }

    // Mouse hints (not configurable)
    help_text.push(Line::from(Span::styled("Mouse", Style::default().bold())));
    help_text.push(Line::from("  Click            Select item"));
    help_text.push(Line::from("  Double-click     Open workspace"));
    help_text.push(Line::from("  Right-click      Action menu"));
    help_text.push(Line::from("  Scroll wheel     Navigate list"));
    help_text.push(Line::from("  Drag tab         Reorder tabs"));

    let total_lines = help_text.len() as u16;
    let inner_height = area.height.saturating_sub(2); // borders
    let max_scroll = total_lines.saturating_sub(inner_height);
    ui.help_scroll = ui.help_scroll.min(max_scroll);
    let scroll = ui.help_scroll;

    let has_more_below = scroll < max_scroll;
    let has_more_above = scroll > 0;
    let title = match (has_more_above, has_more_below) {
        (true, true) => " Help ↑↓ ",
        (false, true) => " Help ↓ ",
        (true, false) => " Help ↑ ",
        (false, false) => " Help ",
    };

    let paragraph = Paragraph::new(help_text)
        .block(Block::bordered().style(theme.block_style()).title(title))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn layout_status_text(ui: &UiState) -> &'static str {
    match ui.view_layout {
        RepoViewLayout::Auto => "LAYOUT AUTO",
        RepoViewLayout::Zoom => "LAYOUT ZOOM",
        RepoViewLayout::Right => "LAYOUT RIGHT",
        RepoViewLayout::Below => "LAYOUT BELOW",
    }
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

fn render_config_screen(model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let host_count = model.hosts.len();
    let host_height = (host_count as u16 + 2).min(8);
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(host_height)])
        .split(chunks[0]);
    render_global_status(model, theme, frame, left_chunks[0]);
    render_hosts_status(model, theme, frame, left_chunks[1]);

    render_event_log(ui, theme, frame, chunks[1]);
}

/// Return the worse of two provider statuses (Error > Ok > None).
fn worse_status(a: Option<ProviderStatus>, b: Option<ProviderStatus>) -> Option<ProviderStatus> {
    match (a, b) {
        (Some(ProviderStatus::Error), _) | (_, Some(ProviderStatus::Error)) => Some(ProviderStatus::Error),
        (Some(ProviderStatus::Ok), _) | (_, Some(ProviderStatus::Ok)) => Some(ProviderStatus::Ok),
        _ => None,
    }
}

fn render_global_status(model: &TuiModel, theme: &Theme, frame: &mut Frame, area: Rect) {
    // Collect providers across all repos: (category_key, provider_name) → status.
    // Collect unique (category, provider_name) pairs with worst-wins status.
    // If a provider is healthy in repo A but failing in repo B, the global
    // view should surface the failure (Error > Ok > None).
    struct ProviderEntry {
        name: String,
        status: Option<ProviderStatus>,
    }
    let mut by_category: HashMap<&str, Vec<ProviderEntry>> = HashMap::new();

    for repo_identity in &model.repo_order {
        let rm = &model.repos[repo_identity];
        for &(_, key) in &PROVIDER_CATEGORIES {
            if let Some(pnames) = rm.provider_names.get(key) {
                let entries = by_category.entry(key).or_default();
                for pname in pnames {
                    let status = model.provider_statuses.get(&(repo_identity.clone(), key.to_string(), pname.clone())).copied();
                    if let Some(existing) = entries.iter_mut().find(|e| e.name == *pname) {
                        // Worst-wins: Error beats Ok beats None.
                        existing.status = worse_status(existing.status, status);
                    } else {
                        entries.push(ProviderEntry { name: pname.clone(), status });
                    }
                }
            }
        }
    }

    let mut rows: Vec<Row> = Vec::new();

    for &(category, key) in &PROVIDER_CATEGORIES {
        let entries = by_category.get(key);
        if let Some(providers) = entries {
            for (i, provider) in providers.iter().enumerate() {
                let label = if i == 0 { category } else { "" };
                rows.push(provider_row(label, &provider.name, provider.status, theme));
            }
        } else {
            rows.push(provider_empty_row(category, theme));
        }
    }

    let table = Table::new(rows, provider_table_widths())
        .header(provider_table_header(theme))
        .block(Block::bordered().style(theme.block_style()).title(" Providers "));
    frame.render_widget(table, area);
}

fn render_hosts_status(model: &TuiModel, theme: &Theme, frame: &mut Frame, area: Rect) {
    // Sort: local first, then peers alphabetically
    let mut hosts: Vec<&TuiHostState> = model.hosts.values().collect();
    hosts.sort_by(|a, b| match (a.is_local, b.is_local) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.host_name.cmp(&b.host_name),
    });

    let rows: Vec<Row> =
        hosts
            .iter()
            .map(|h| {
                let (icon, icon_style) = match h.status {
                    PeerStatus::Connected => ("\u{25cf}", Style::default().fg(theme.status_ok)),
                    PeerStatus::Disconnected => ("\u{25cb}", Style::default().fg(theme.error)),
                    PeerStatus::Connecting => ("\u{25d0}", Style::default().fg(theme.warning)),
                    PeerStatus::Reconnecting => ("\u{25d0}", Style::default().fg(theme.warning)),
                    PeerStatus::Rejected => ("\u{2717}", Style::default().fg(theme.error)),
                };

                let name = if h.is_local { format!("{} (local)", h.host_name) } else { h.host_name.to_string() };

                let sys = &h.summary.system;
                let os_arch = match (sys.os.as_deref(), sys.arch.as_deref()) {
                    (Some(os), Some(arch)) => format!("{os}/{arch}"),
                    (Some(os), None) => os.to_string(),
                    _ => "\u{2014}".to_string(),
                };
                let cpus = sys.cpu_count.map_or("\u{2014}".to_string(), |c| format!("{c} CPUs"));
                let mem = sys.memory_total_mb.map_or("\u{2014}".to_string(), |m| {
                    if m >= 1024 {
                        format!("{} GB", m / 1024)
                    } else {
                        format!("{m} MB")
                    }
                });

                let providers: String = h
                    .summary
                    .providers
                    .iter()
                    .map(|p| {
                        let check = if p.healthy { "\u{2713}" } else { "\u{2717}" };
                        format!("{} {check}", p.name)
                    })
                    .collect::<Vec<_>>()
                    .join("  ");

                Row::new(vec![
                    Cell::from(Span::styled(format!("{icon} "), icon_style)),
                    Cell::from(name),
                    Cell::from(os_arch),
                    Cell::from(cpus),
                    Cell::from(mem),
                    Cell::from(providers),
                ])
            })
            .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Fill(1),
    ];

    let table = Table::new(rows, widths).block(Block::bordered().style(theme.block_style()).title(" Hosts "));
    frame.render_widget(table, area);
}

fn render_event_log(ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
    use event_log::DisplayEntry;

    let filter = ui.event_log.filter;
    let entries = event_log::get_entries(&filter);
    let entry_count = entries.len();

    if entry_count != ui.event_log.count {
        ui.event_log.count = entry_count;
        if entry_count > 0 {
            ui.event_log.selected = Some(entry_count - 1);
        }
    }

    let items: Vec<ListItem> = entries
        .iter()
        .map(|display_entry| match display_entry {
            DisplayEntry::Log(entry) => {
                let (h, m, s) = entry.hms;
                let timestamp = format!("{h:02}:{m:02}:{s:02}");

                let level_style = theme.log_level_style(entry.level.as_str());

                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", timestamp), Style::default().fg(theme.muted)),
                    Span::styled(format!("{:<5} ", entry.level), level_style),
                    Span::raw(&entry.message),
                ]))
            }
            DisplayEntry::RetentionMarker(level) => {
                ListItem::new(Line::from(Span::styled(format!("── {level} retention starts here ──"), Style::default().fg(theme.muted))))
            }
        })
        .collect();

    let filter_label = format!(" {} ", filter.filter_label());
    let filter_label_len = filter_label.len() as u16;
    let filter_x = area.x + area.width.saturating_sub(filter_label_len + 1);
    ui.layout.event_log_filter_area = Rect::new(filter_x, area.y, filter_label_len, 1);

    let list = List::new(items)
        .block(
            Block::bordered()
                .style(theme.block_style())
                .title(" Event Log ")
                .title_top(Line::from(Span::styled(filter_label, Style::default().fg(theme.muted))).right_aligned()),
        )
        .highlight_style(Style::default().bg(theme.multi_select_bg));

    let mut state = ListState::default();
    state.select(ui.event_log.selected);
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::HostName;

    use super::*;
    use crate::app::{RepoViewLayout, UiState};

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

    #[test]
    fn normal_mode_key_chips_show_local_target_host_by_default() {
        let ui = UiState::new(&[]);

        let host_chip =
            normal_mode_key_chips(&ui).into_iter().find(|chip| chip.action == StatusBarAction::key(KeyCode::Char('h'))).expect("host chip");

        assert_eq!(host_chip.label, "Host Local");
    }

    #[test]
    fn normal_mode_key_chips_show_selected_remote_target_host() {
        let mut ui = UiState::new(&[]);
        ui.target_host = Some(HostName::new("alpha"));

        let host_chip =
            normal_mode_key_chips(&ui).into_iter().find(|chip| chip.action == StatusBarAction::key(KeyCode::Char('h'))).expect("host chip");

        assert_eq!(host_chip.label, "Host alpha");
    }
}
