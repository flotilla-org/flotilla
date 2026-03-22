use std::{any::Any, collections::HashMap};

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::{InFlightCommand, RepoViewLayout, TuiModel, UiState},
    binding_table::{KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    segment_bar::{self, BarStyle, ThemedRibbonStyle},
    shimmer::shimmer_spans,
    status_bar::{
        KeyChip, ModeIndicator, StatusBarAction, StatusBarInput, StatusBarModel, StatusBarTarget, StatusSection, TaskSection,
        DEFAULT_STATUS_WIDTH_BUDGET,
    },
    theme::Theme,
};

/// Standalone status bar component. Handles rendering and mouse hit-testing
/// for the bottom status strip.
///
/// This is a pure renderer: all content resolution (status text, key chips,
/// task spinner, error items, mode indicators) is performed by `Screen`
/// before calling `render_bespoke`.
#[derive(Default)]
pub struct StatusBarWidget {
    /// Click targets for key chips, populated during render.
    pub(crate) key_targets: Vec<StatusBarTarget>,
    /// Click targets for dismiss (error clear) buttons, populated during render.
    pub(crate) dismiss_targets: Vec<StatusBarTarget>,
    /// The area occupied by the status bar during the last render.
    area: Rect,
}

impl StatusBarWidget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the status bar into `area` with pre-resolved content.
    ///
    /// All content resolution is performed by the caller (Screen). This
    /// method only handles layout and rendering.
    #[allow(clippy::too_many_arguments)]
    pub fn render_bespoke(
        &mut self,
        status: StatusSection,
        key_chips: Vec<KeyChip>,
        task: Option<TaskSection>,
        error_items: Vec<crate::app::VisibleStatusItem>,
        mode_indicators: Vec<ModeIndicator>,
        show_keys: bool,
        theme: &Theme,
        frame: &mut Frame,
        area: Rect,
    ) {
        self.area = area;
        self.key_targets.clear();
        self.dismiss_targets.clear();

        // Error items take priority over the fragment's status.
        let status_section =
            if let Some(item) = error_items.into_iter().next() { StatusSection::error(item.id, &item.text) } else { status };

        let status_section_clone = status_section.clone();
        let status_model = StatusBarModel::build(StatusBarInput {
            width: area.width as usize,
            preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET.min(area.width as usize),
            keys_visible: show_keys,
            status: status_section,
            task,
            keys: key_chips,
            mode_indicators,
        });

        frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

        let mut spans = Vec::new();
        let mut x = 0usize;
        let status_style = match status_section_clone {
            StatusSection::Error { .. } => Style::default().fg(theme.status_error).bg(theme.bar_bg).bold(),
            StatusSection::Plain(_) => Style::default().fg(theme.text).bg(theme.bar_bg),
        };

        if !status_model.status_text.is_empty() {
            let status_width = status_model.status_text.width();
            spans.push(Span::styled(status_model.status_text.clone(), status_style));
            if let Some(id) = status_section_clone.dismiss_id() {
                self.dismiss_targets.push(StatusBarTarget::new(
                    Rect::new(area.x + status_width.saturating_sub(1) as u16, area.y, 1, 1),
                    StatusBarAction::ClearError(id),
                ));
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

            self.key_targets
                .push(StatusBarTarget::new(Rect::new(area.x + ribbon_start as u16, area.y, rendered.width as u16, 1), chip.action.clone()));
            x += rendered.width;
        }

        // Mode indicators (compact, after keys)
        if x < status_model.mode_start {
            spans.push(Span::styled(" ".repeat(status_model.mode_start - x), Style::default().fg(theme.text).bg(theme.bar_bg)));
            x = status_model.mode_start;
        }

        let icon_style = Style::default().fg(theme.key_hint).bg(theme.bar_bg);
        let label_style = Style::default().fg(theme.muted).bg(theme.bar_bg);
        for indicator in &status_model.mode_indicators {
            let indicator_start = x;
            spans.push(Span::styled(format!(" {}", indicator.icon), icon_style));
            spans.push(Span::styled(format!(" {} ", indicator.label), label_style));
            let indicator_width = indicator.width();
            self.key_targets.push(StatusBarTarget::new(
                Rect::new(area.x + indicator_start as u16, area.y, indicator_width as u16, 1),
                indicator.action.clone(),
            ));
            x += indicator_width;
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

    /// Hit-test a left mouse click against the rendered status bar targets.
    ///
    /// Returns `Some(action)` if a target was hit, `None` otherwise.
    pub fn handle_click(&self, x: u16, y: u16) -> Option<StatusBarAction> {
        // Check dismiss targets first (error clear buttons)
        for target in &self.dismiss_targets {
            if target.contains(x, y) {
                return Some(target.action.clone());
            }
        }

        // Check key chip targets
        for target in &self.key_targets {
            if target.contains(x, y) {
                return Some(target.action.clone());
            }
        }

        None
    }
}

impl InteractiveWidget for StatusBarWidget {
    fn handle_action(&mut self, _action: Action, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Outcome::Ignored;
        }

        match self.handle_click(mouse.column, mouse.row) {
            Some(StatusBarAction::KeyPress { code, modifiers }) => {
                ctx.app_actions.push(AppAction::StatusBarKeyPress { code, modifiers });
                Outcome::Consumed
            }
            Some(StatusBarAction::ClearError(id)) => {
                ctx.app_actions.push(AppAction::ClearError(id));
                Outcome::Consumed
            }
            None => Outcome::Ignored,
        }
    }

    fn render(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut RenderContext) {
        // Status bar rendering is driven by Screen::render() which calls
        // render_bespoke() directly with pre-resolved content.
        // This InteractiveWidget::render() is not used.
    }

    fn binding_mode(&self) -> KeyBindingMode {
        crate::binding_table::BindingModeId::Normal.into()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ── Utility functions (pub(crate) for Screen to use) ──────────────────

/// Resolve the active in-flight task description for the current repo.
pub(crate) fn active_task(model: &TuiModel, in_flight: &HashMap<u64, InFlightCommand>) -> Option<TaskSection> {
    let active_repo = &model.repo_order[model.active_repo];
    let repo_cmds: Vec<(&u64, &InFlightCommand)> = in_flight.iter().filter(|(_, cmd)| &cmd.repo_identity == active_repo).collect();

    // Highest command ID = most recently started (IDs are monotonically increasing AtomicU64).
    let (_, most_recent) = repo_cmds.iter().max_by_key(|(id, _)| *id)?;
    let description = if repo_cmds.len() <= 1 {
        most_recent.description.clone()
    } else {
        format!("{} (+{})", most_recent.description, repo_cmds.len() - 1)
    };

    Some(TaskSection::new(&description, 0))
}

/// Build mode indicators for Normal mode (layout and host).
pub(crate) fn normal_mode_indicators(ui: &UiState) -> Vec<ModeIndicator> {
    let layout_icon = match ui.view_layout {
        RepoViewLayout::Auto => "◫",
        RepoViewLayout::Zoom => "□",
        RepoViewLayout::Right => "▥",
        RepoViewLayout::Below => "▤",
    };
    let layout_label = match ui.view_layout {
        RepoViewLayout::Auto => "auto",
        RepoViewLayout::Zoom => "zoom",
        RepoViewLayout::Right => "right",
        RepoViewLayout::Below => "below",
    };

    let host_label = match ui.target_host.as_ref() {
        Some(host) => format!("@{host}"),
        None => "@local".into(),
    };

    vec![
        ModeIndicator::new(layout_icon, layout_label, StatusBarAction::key(KeyCode::Char('l'))),
        ModeIndicator::new("", &host_label, StatusBarAction::key(KeyCode::Char('h'))),
    ]
}

/// Build a `StatusSection` from a `StatusFragment`, resolving the fragment's content.
///
/// - `Label(s)` → `StatusSection::Plain(s)`
/// - `ActiveInput { prefix, text }` → `StatusSection::Plain(format!("{prefix}{text}"))`
/// - `Progress { label, .. }` → `StatusSection::Plain(label)` (task spinner shows text)
/// - `None` → `StatusSection::Plain(fallback)`
pub(crate) fn resolve_status_section(fragment: &StatusFragment, fallback: &str) -> StatusSection {
    match &fragment.status {
        Some(StatusContent::Label(label)) => StatusSection::plain(label),
        Some(StatusContent::ActiveInput { prefix, text }) => StatusSection::plain(&format!("{prefix}{text}")),
        Some(StatusContent::Progress { label, .. }) => StatusSection::plain(label),
        None => StatusSection::plain(fallback),
    }
}

/// Extract task from a `StatusFragment::Progress` variant.
pub(crate) fn resolve_task_from_fragment(fragment: &StatusFragment) -> Option<TaskSection> {
    match &fragment.status {
        Some(StatusContent::Progress { text, .. }) => Some(TaskSection::new(text, 0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::RepoLabels;
    use ratatui::layout::Rect;

    use super::*;
    use crate::{app::test_support::repo_info, status_bar::StatusBarAction};

    #[test]
    fn handle_click_returns_none_for_miss() {
        let widget = StatusBarWidget::new();
        assert!(widget.handle_click(100, 100).is_none());
    }

    #[test]
    fn handle_click_detects_dismiss_target() {
        let mut widget = StatusBarWidget::new();
        widget.dismiss_targets.push(StatusBarTarget::new(Rect::new(5, 10, 3, 1), StatusBarAction::ClearError(42)));
        let action = widget.handle_click(6, 10);
        assert_eq!(action, Some(StatusBarAction::ClearError(42)));
    }

    #[test]
    fn handle_click_detects_key_target() {
        let mut widget = StatusBarWidget::new();
        widget.key_targets.push(StatusBarTarget::new(Rect::new(20, 10, 8, 1), StatusBarAction::key(KeyCode::Char('q'))));
        let action = widget.handle_click(24, 10);
        assert_eq!(action, Some(StatusBarAction::key(KeyCode::Char('q'))));
    }

    #[test]
    fn dismiss_target_takes_priority_over_key_target() {
        let mut widget = StatusBarWidget::new();
        // Overlapping targets
        widget.dismiss_targets.push(StatusBarTarget::new(Rect::new(5, 10, 10, 1), StatusBarAction::ClearError(1)));
        widget.key_targets.push(StatusBarTarget::new(Rect::new(5, 10, 10, 1), StatusBarAction::key(KeyCode::Char('q'))));
        let action = widget.handle_click(8, 10);
        assert_eq!(action, Some(StatusBarAction::ClearError(1)));
    }

    #[test]
    fn active_task_shows_most_recent_command_with_count_suffix() {
        let ri = repo_info("/tmp/test-repo", "test-repo", RepoLabels::default());
        let model = TuiModel::from_repo_info(vec![ri]);
        let repo_identity = model.repo_order[0].clone();

        let mut in_flight = HashMap::new();
        in_flight.insert(10, InFlightCommand {
            repo_identity: repo_identity.clone(),
            repo: PathBuf::from("/tmp/test-repo"),
            description: "older command".into(),
        });
        in_flight.insert(20, InFlightCommand { repo_identity, repo: PathBuf::from("/tmp/test-repo"), description: "newer command".into() });

        let task = active_task(&model, &in_flight).expect("should have an active task");
        assert!(
            task.description.contains("newer command"),
            "task description should show the most recent command, got: {}",
            task.description
        );
        assert!(task.description.contains("(+1)"), "task description should show (+1) count suffix, got: {}", task.description);
    }

    #[test]
    fn active_task_shows_single_command_without_suffix() {
        let ri = repo_info("/tmp/test-repo", "test-repo", RepoLabels::default());
        let model = TuiModel::from_repo_info(vec![ri]);
        let repo_identity = model.repo_order[0].clone();

        let mut in_flight = HashMap::new();
        in_flight.insert(42, InFlightCommand { repo_identity, repo: PathBuf::from("/tmp/test-repo"), description: "only command".into() });

        let task = active_task(&model, &in_flight).expect("should have an active task");
        assert_eq!(task.description, "only command");
    }

    #[test]
    fn active_task_returns_none_when_no_commands() {
        let ri = repo_info("/tmp/test-repo", "test-repo", RepoLabels::default());
        let model = TuiModel::from_repo_info(vec![ri]);
        let in_flight: HashMap<u64, InFlightCommand> = HashMap::new();
        assert!(active_task(&model, &in_flight).is_none());
    }
}
