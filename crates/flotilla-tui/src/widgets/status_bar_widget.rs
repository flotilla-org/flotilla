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
    app::{collect_visible_status_items, InFlightCommand, RepoViewLayout, TuiModel, UiMode, UiState},
    binding_table::BindingModeId,
    keymap::Action,
    segment_bar::{self, BarStyle, ThemedRibbonStyle},
    shimmer::shimmer_spans,
    status_bar::{
        KeyChip, ModeIndicator, StatusBarAction, StatusBarInput, StatusBarModel, StatusBarTarget, StatusSection, TaskSection,
        DEFAULT_STATUS_WIDTH_BUDGET,
    },
    theme::Theme,
    widgets::WidgetStatusData,
};

const ENTER_KEY_GLYPH: &str = "ENT";

/// Content computed for a single status bar render pass.
struct StatusBarContent {
    status: StatusSection,
    keys: Vec<KeyChip>,
    task: Option<TaskSection>,
    mode_indicators: Vec<ModeIndicator>,
}

/// Standalone status bar component. Handles rendering and mouse hit-testing
/// for the bottom status strip.
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

    /// Render the status bar into `area`. Click targets are stored on
    /// `self` for later hit-testing via `handle_click`.
    #[allow(clippy::too_many_arguments)]
    pub fn render_bespoke(
        &mut self,
        model: &TuiModel,
        ui: &mut UiState,
        in_flight: &HashMap<u64, InFlightCommand>,
        theme: &Theme,
        frame: &mut Frame,
        area: Rect,
        active_widget_mode: Option<BindingModeId>,
        active_widget_data: WidgetStatusData,
    ) {
        self.area = area;
        self.key_targets.clear();
        self.dismiss_targets.clear();

        let content = status_bar_content(model, ui, in_flight, active_widget_mode, &active_widget_data);
        let status_section = content.status.clone();
        let status_model = StatusBarModel::build(StatusBarInput {
            width: area.width as usize,
            preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET.min(area.width as usize),
            keys_visible: ui.status_bar.show_keys,
            status: content.status,
            task: content.task,
            keys: content.keys,
            mode_indicators: content.mode_indicators,
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

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        self.render_bespoke(
            ctx.model,
            ctx.ui,
            ctx.in_flight,
            ctx.theme,
            frame,
            area,
            ctx.active_widget_mode,
            ctx.active_widget_data.clone(),
        );
    }

    fn mode_id(&self) -> BindingModeId {
        BindingModeId::Normal
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ── Content computation (moved from ui.rs) ───────────────────────────

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

fn normal_mode_key_chips() -> Vec<KeyChip> {
    vec![
        key_chip(ENTER_KEY_GLYPH, "Open", KeyCode::Enter),
        key_chip(".", "Menu", KeyCode::Char('.')),
        key_chip("n", "New", KeyCode::Char('n')),
        key_chip("?", "Help", KeyCode::Char('?')),
        key_chip("q", "Quit", KeyCode::Char('q')),
    ]
}

fn normal_mode_indicators(ui: &UiState) -> Vec<ModeIndicator> {
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

fn key_chip(key: &str, label: &str, code: KeyCode) -> KeyChip {
    KeyChip::new(key, label, StatusBarAction::key(code))
}

fn active_rui<'a>(model: &TuiModel, ui: &'a UiState) -> &'a crate::app::RepoUiState {
    ui.active_repo_ui(&model.repo_order, model.active_repo)
}

fn status_bar_content(
    model: &TuiModel,
    ui: &UiState,
    in_flight: &HashMap<u64, InFlightCommand>,
    active_widget_mode: Option<BindingModeId>,
    active_widget_data: &WidgetStatusData,
) -> StatusBarContent {
    let visible_error = collect_visible_status_items(model, ui).into_iter().next();

    // Widget stack overrides UiMode for status bar content.
    if let Some(widget_mode) = active_widget_mode {
        match widget_mode {
            BindingModeId::Help => {
                return StatusBarContent {
                    status: StatusSection::plain("HELP"),
                    keys: vec![
                        key_chip("j", "Down", KeyCode::Char('j')),
                        key_chip("k", "Up", KeyCode::Char('k')),
                        key_chip("ESC", "Close", KeyCode::Esc),
                        key_chip("?", "Close", KeyCode::Char('?')),
                    ],
                    task: None,
                    mode_indicators: vec![],
                };
            }
            BindingModeId::ActionMenu => {
                return StatusBarContent {
                    status: StatusSection::plain("ACTIONS"),
                    keys: vec![
                        key_chip("j", "Down", KeyCode::Char('j')),
                        key_chip("k", "Up", KeyCode::Char('k')),
                        key_chip(ENTER_KEY_GLYPH, "Select", KeyCode::Enter),
                        key_chip("ESC", "Close", KeyCode::Esc),
                    ],
                    task: None,
                    mode_indicators: vec![],
                };
            }
            BindingModeId::DeleteConfirm => {
                return StatusBarContent {
                    status: StatusSection::plain("CONFIRM DELETE"),
                    keys: vec![key_chip("y", "Yes", KeyCode::Char('y')), key_chip("n", "No", KeyCode::Char('n'))],
                    task: None,
                    mode_indicators: vec![],
                };
            }
            BindingModeId::CloseConfirm => {
                return StatusBarContent {
                    status: StatusSection::plain("CONFIRM CLOSE"),
                    keys: vec![key_chip("y", "Yes", KeyCode::Char('y')), key_chip("n", "No", KeyCode::Char('n'))],
                    task: None,
                    mode_indicators: vec![],
                };
            }
            BindingModeId::BranchInput => {
                let generating = matches!(active_widget_data, WidgetStatusData::BranchInput { generating: true });
                return if generating {
                    StatusBarContent {
                        status: StatusSection::plain("NEW BRANCH"),
                        keys: vec![],
                        task: Some(TaskSection::new("Generating branch name...", 0)),
                        mode_indicators: vec![],
                    }
                } else {
                    StatusBarContent {
                        status: StatusSection::plain("NEW BRANCH"),
                        keys: vec![key_chip(ENTER_KEY_GLYPH, "Create", KeyCode::Enter), key_chip("ESC", "Cancel", KeyCode::Esc)],
                        task: None,
                        mode_indicators: vec![],
                    }
                };
            }
            BindingModeId::IssueSearch => {
                let query = if let UiMode::IssueSearch { ref input } = ui.mode { input.value().to_string() } else { String::new() };
                return StatusBarContent {
                    status: StatusSection::plain(&format!("SEARCH {}", query)),
                    keys: vec![key_chip(ENTER_KEY_GLYPH, "Apply", KeyCode::Enter), key_chip("ESC", "Cancel", KeyCode::Esc)],
                    task: None,
                    mode_indicators: vec![],
                };
            }
            BindingModeId::CommandPalette => {
                let input_text = match active_widget_data {
                    WidgetStatusData::CommandPalette { input_text } => input_text.clone(),
                    _ => String::new(),
                };
                let status_text = format!("/{}", input_text);
                return StatusBarContent {
                    status: StatusSection::plain(&status_text),
                    keys: vec![
                        key_chip(ENTER_KEY_GLYPH, "Run", KeyCode::Enter),
                        key_chip("TAB", "Fill", KeyCode::Tab),
                        key_chip("ESC", "Close", KeyCode::Esc),
                    ],
                    task: None,
                    mode_indicators: normal_mode_indicators(ui),
                };
            }
            BindingModeId::FilePicker => {
                return StatusBarContent {
                    status: StatusSection::plain("ADD REPO"),
                    keys: vec![
                        key_chip("j", "Down", KeyCode::Char('j')),
                        key_chip("k", "Up", KeyCode::Char('k')),
                        key_chip("tab", "Complete", KeyCode::Tab),
                        key_chip(ENTER_KEY_GLYPH, "Select", KeyCode::Enter),
                        key_chip("ESC", "Cancel", KeyCode::Esc),
                    ],
                    task: None,
                    mode_indicators: vec![],
                };
            }
            _ => {} // Other widget modes will be handled as they are extracted
        }
    }

    // Legacy UiMode fallback — only reached when no widget-mode override matched.
    // The remaining UiMode variants that have no widget on the stack are Normal,
    // Config, and IssueSearch (which still uses the bridge for status bar text).
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
                StatusSection::plain("/ for commands")
            };

            let task = active_task(model, in_flight).map(|(description, spinner_index)| TaskSection::new(&description, spinner_index));
            let mut keys = normal_mode_key_chips();
            if rui.active_search_query.is_some() {
                keys.insert(0, key_chip("ESC", "Clear", KeyCode::Esc));
            }
            StatusBarContent { status, keys, task, mode_indicators: normal_mode_indicators(ui) }
        }
        UiMode::Config => StatusBarContent {
            status: StatusSection::plain("FLOTILLA"),
            keys: vec![
                key_chip("j", "Down", KeyCode::Char('j')),
                key_chip("k", "Up", KeyCode::Char('k')),
                key_chip("[", "Prev", KeyCode::Char('[')),
                key_chip("]", "Next", KeyCode::Char(']')),
                key_chip("q", "Quit", KeyCode::Char('q')),
            ],
            task: None,
            mode_indicators: vec![],
        },
        UiMode::IssueSearch { input } => StatusBarContent {
            status: StatusSection::plain(&format!("SEARCH {}", input.value())),
            keys: vec![key_chip(ENTER_KEY_GLYPH, "Apply", KeyCode::Enter), key_chip("ESC", "Cancel", KeyCode::Esc)],
            task: None,
            mode_indicators: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;
    use crate::status_bar::StatusBarAction;

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
}
