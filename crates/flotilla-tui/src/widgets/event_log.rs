use std::{any::Any, collections::HashMap};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Cell, List, ListItem, ListState, Row, Table},
    Frame,
};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::{PeerStatus, ProviderStatus, TuiHostState, TuiModel},
    binding_table::BindingModeId,
    event_log::{self, LevelExt},
    keymap::Action,
    theme::Theme,
};

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

/// Standalone event log / config screen component.
///
/// Owns the event log selection state and filter level. Renders the
/// config screen (providers, hosts, log) and handles navigation and
/// filter click detection.
pub struct EventLogWidget {
    pub selected: Option<usize>,
    pub count: usize,
    pub filter: tracing::Level,
    /// Layout area for the filter label click target, populated during render.
    filter_area: Rect,
}

impl Default for EventLogWidget {
    fn default() -> Self {
        Self { selected: None, count: 0, filter: tracing::Level::INFO, filter_area: Rect::default() }
    }
}

impl EventLogWidget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render just the event log panel (scrollable list with level filter).
    pub fn render_event_log(&mut self, theme: &Theme, frame: &mut Frame, area: Rect) {
        use event_log::DisplayEntry;

        let filter = self.filter;
        let entries = event_log::get_entries(&filter);
        let entry_count = entries.len();

        if entry_count != self.count {
            self.count = entry_count;
            if entry_count > 0 {
                self.selected = Some(entry_count - 1);
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
                DisplayEntry::RetentionMarker(level) => ListItem::new(Line::from(Span::styled(
                    format!("── {level} retention starts here ──"),
                    Style::default().fg(theme.muted),
                ))),
            })
            .collect();

        let filter_label = format!(" {} ", filter.filter_label());
        let filter_label_len = filter_label.len() as u16;
        let filter_x = area.x + area.width.saturating_sub(filter_label_len + 1);
        self.filter_area = Rect::new(filter_x, area.y, filter_label_len, 1);

        let list = List::new(items)
            .block(
                Block::bordered()
                    .style(theme.block_style())
                    .title(" Event Log ")
                    .title_top(Line::from(Span::styled(filter_label, Style::default().fg(theme.muted))).right_aligned()),
            )
            .highlight_style(Style::default().bg(theme.multi_select_bg));

        let mut state = ListState::default();
        state.select(self.selected);
        frame.render_stateful_widget(list, area, &mut state);
    }

    /// Check if a mouse click hits the filter label. Returns `true` if the
    /// filter was cycled, `false` otherwise.
    pub fn handle_click(&mut self, x: u16, y: u16) -> bool {
        let f = self.filter_area;
        if f.width > 0 && x >= f.x && x < f.x + f.width && y >= f.y && y < f.y + f.height {
            self.filter = self.filter.cycle();
            self.count = 0;
            true
        } else {
            false
        }
    }

    /// Move selection down one entry.
    pub fn select_next(&mut self) {
        if let Some(sel) = self.selected {
            if sel + 1 < self.count {
                self.selected = Some(sel + 1);
            }
        } else if self.count > 0 {
            self.selected = Some(self.count - 1);
        }
    }

    /// Move selection up one entry.
    pub fn select_prev(&mut self) {
        if let Some(sel) = self.selected {
            if sel > 0 {
                self.selected = Some(sel - 1);
            }
        }
    }

    /// The filter click area, for external code that still needs it (e.g.
    /// backward-compatible layout areas or tab bar click detection).
    pub fn filter_area(&self) -> Rect {
        self.filter_area
    }
}

impl InteractiveWidget for EventLogWidget {
    fn handle_action(&mut self, action: Action, _ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.select_next();
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.select_prev();
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _ctx: &mut WidgetContext) -> Outcome {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.handle_click(mouse.column, mouse.row) {
                    Outcome::Consumed
                } else {
                    Outcome::Ignored
                }
            }
            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        let host_count = ctx.model.hosts.len();
        let host_height = (host_count as u16 + 2).min(8);
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(host_height)])
            .split(chunks[0]);
        render_global_status(ctx.model, ctx.theme, frame, left_chunks[0]);
        render_hosts_status(ctx.model, ctx.theme, frame, left_chunks[1]);

        self.render_event_log(ctx.theme, frame, chunks[1]);
    }

    fn mode_id(&self) -> BindingModeId {
        BindingModeId::Overview
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ── Provider table helpers ────────────────────────────────────────────

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

/// Return the worse of two provider statuses (Error > Ok > None).
fn worse_status(a: Option<ProviderStatus>, b: Option<ProviderStatus>) -> Option<ProviderStatus> {
    match (a, b) {
        (Some(ProviderStatus::Error), _) | (_, Some(ProviderStatus::Error)) => Some(ProviderStatus::Error),
        (Some(ProviderStatus::Ok), _) | (_, Some(ProviderStatus::Ok)) => Some(ProviderStatus::Ok),
        _ => None,
    }
}

// ── Config screen sub-panels ──────────────────────────────────────────

fn render_global_status(model: &TuiModel, theme: &Theme, frame: &mut Frame, area: Rect) {
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

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn default_state() {
        let w = EventLogWidget::new();
        assert!(w.selected.is_none());
        assert_eq!(w.count, 0);
        assert_eq!(w.filter, tracing::Level::INFO);
        assert_eq!(w.filter_area, Rect::default());
    }

    // ── select_next ───────────────────────────────────────────────────

    #[test]
    fn select_next_advances() {
        let mut w = EventLogWidget { selected: Some(2), count: 10, ..Default::default() };
        w.select_next();
        assert_eq!(w.selected, Some(3));
    }

    #[test]
    fn select_next_clamps_at_end() {
        let mut w = EventLogWidget { selected: Some(9), count: 10, ..Default::default() };
        w.select_next();
        assert_eq!(w.selected, Some(9));
    }

    #[test]
    fn select_next_from_none_jumps_to_last() {
        let mut w = EventLogWidget { selected: None, count: 5, ..Default::default() };
        w.select_next();
        assert_eq!(w.selected, Some(4));
    }

    #[test]
    fn select_next_from_none_empty_stays_none() {
        let mut w = EventLogWidget { selected: None, count: 0, ..Default::default() };
        w.select_next();
        assert!(w.selected.is_none());
    }

    // ── select_prev ───────────────────────────────────────────────────

    #[test]
    fn select_prev_decrements() {
        let mut w = EventLogWidget { selected: Some(5), count: 10, ..Default::default() };
        w.select_prev();
        assert_eq!(w.selected, Some(4));
    }

    #[test]
    fn select_prev_clamps_at_zero() {
        let mut w = EventLogWidget { selected: Some(0), count: 10, ..Default::default() };
        w.select_prev();
        assert_eq!(w.selected, Some(0));
    }

    #[test]
    fn select_prev_from_none_stays_none() {
        let mut w = EventLogWidget { selected: None, count: 10, ..Default::default() };
        w.select_prev();
        assert!(w.selected.is_none());
    }

    // ── handle_click ──────────────────────────────────────────────────

    #[test]
    fn handle_click_inside_filter_cycles() {
        let mut w = EventLogWidget { filter: tracing::Level::INFO, filter_area: Rect::new(50, 0, 6, 1), ..Default::default() };
        w.count = 5;
        assert!(w.handle_click(53, 0));
        assert_eq!(w.filter, tracing::Level::DEBUG);
        assert_eq!(w.count, 0);
    }

    #[test]
    fn handle_click_outside_filter_returns_false() {
        let mut w = EventLogWidget { filter: tracing::Level::INFO, filter_area: Rect::new(50, 0, 6, 1), ..Default::default() };
        assert!(!w.handle_click(10, 0));
        assert_eq!(w.filter, tracing::Level::INFO);
    }

    #[test]
    fn handle_click_empty_filter_area_returns_false() {
        let mut w = EventLogWidget::new();
        assert!(!w.handle_click(0, 0));
    }
}
