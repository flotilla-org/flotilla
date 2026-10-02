use std::any::Any;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_commands::{HostResolution, RepoContext, Resolved};
use flotilla_protocol::{Command, NodeId, ProvisioningTarget};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};
use unicode_width::UnicodeWidthStr;

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::{file_picker_start_dir, ProjectAddressState, TuiModel},
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    palette::{self, PaletteCompletion, PaletteInputState, PaletteLocalResult, PaletteParseResult, MAX_PALETTE_ROWS},
};

pub struct CommandPaletteWidget {
    input: Input,
    selected: usize,
    scroll_top: usize,
    target_node_id: Option<NodeId>,
    overlay: Option<crate::ui_helpers::BottomAnchoredOverlayLayout>,
    project_load_requested: bool,
    completion_rows: usize,
    hint_rows: u16,
}

fn completion_display_text(completion: &PaletteCompletion, showing_addresses: bool) -> &str {
    if showing_addresses {
        completion.description.as_str()
    } else {
        completion.value.as_str()
    }
}
impl Default for CommandPaletteWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPaletteWidget {
    pub fn new() -> Self {
        Self {
            input: Input::default(),
            selected: 0,
            scroll_top: 0,
            target_node_id: None,
            overlay: None,
            project_load_requested: false,
            completion_rows: MAX_PALETTE_ROWS,
            hint_rows: 0,
        }
    }

    /// Create a palette widget with pre-filled input text and selection.
    pub fn with_state(input: Input, selected: usize, scroll_top: usize) -> Self {
        Self {
            input,
            selected,
            scroll_top,
            target_node_id: None,
            overlay: None,
            project_load_requested: false,
            completion_rows: MAX_PALETTE_ROWS,
            hint_rows: 0,
        }
    }

    pub fn with_prefill_on_node(text: impl AsRef<str>, target_node_id: Option<NodeId>) -> Self {
        Self {
            input: Input::from(text.as_ref()),
            selected: 0,
            scroll_top: 0,
            target_node_id,
            overlay: None,
            project_load_requested: false,
            completion_rows: MAX_PALETTE_ROWS,
            hint_rows: 0,
        }
    }

    fn request_project_addresses(&mut self, ctx: &mut WidgetContext<'_>) {
        if palette::is_open_address_completion(self.input.value())
            && !self.project_load_requested
            && matches!(
                ctx.model.project_address_state,
                ProjectAddressState::Unloaded | ProjectAddressState::Loaded(_) | ProjectAddressState::Failed
            )
        {
            self.project_load_requested = true;
            ctx.app_actions.push(AppAction::LoadProjectAddresses);
        }
    }

    /// Current input text (for tests / introspection).
    pub fn input_value(&self) -> &str {
        self.input.value()
    }

    /// Compute position-aware completions using model context.
    fn completions(
        &self,
        model: &TuiModel,
        namespaces: &crate::app::NamespaceMap,
        interactions: crate::interaction::InteractionContext<'_>,
    ) -> Vec<PaletteCompletion> {
        palette::palette_completions_with_availability(self.input.value(), model, namespaces, |action| interactions.is_available(action))
    }

    /// Fill the selected completion value into the input, appending to the
    /// existing prefix (everything before the token being completed).
    fn fill_completion(&mut self, completion: &PaletteCompletion) {
        let input = self.input.value();
        let trailing_space = input.ends_with(' ');
        let tokens = palette::tokenize_palette_input(input).unwrap_or_default();

        // Determine prefix: everything before the token being completed.
        let prefix = if trailing_space || tokens.is_empty() {
            // Cursor is after a space — completion replaces nothing, just append.
            input.to_string()
        } else {
            // The last token is a partial — slice input at its start offset.
            let last = tokens.last().expect("tokens is non-empty");
            input[..last.offset].to_string()
        };

        let filled = format!("{}{} ", prefix, completion.value);
        self.input = Input::from(filled.as_str());
        self.selected = 0;
        self.scroll_top = 0;
    }

    fn adjust_scroll(&mut self) {
        let max_visible = self.completion_rows.max(1);
        if self.selected >= self.scroll_top + max_visible {
            self.scroll_top = self.selected.saturating_sub(max_visible - 1);
        } else if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        }
    }

    fn confirm(&mut self, ctx: &mut WidgetContext) -> Outcome {
        let text = self.input.value().to_string();
        if palette::palette_input_state(&text) != PaletteInputState::Ready {
            return Outcome::Consumed;
        }

        match palette::parse_palette_input(&text) {
            Ok(PaletteParseResult::Local(local)) => self.dispatch_local(local, ctx),
            Ok(PaletteParseResult::Resolved(resolved)) => self.dispatch_resolved(resolved, ctx),
            Err(_) => Outcome::Consumed,
        }
    }

    fn dispatch_local(&mut self, local: PaletteLocalResult<'_>, ctx: &mut WidgetContext) -> Outcome {
        match local {
            PaletteLocalResult::Action(action) => self.dispatch_palette_action(action, ctx),
            PaletteLocalResult::SetTheme(name) => {
                ctx.app_actions.push(AppAction::SetTheme(name.to_string()));
                Outcome::Finished
            }
            PaletteLocalResult::SetTarget(name) => {
                ctx.app_actions.push(AppAction::SetTarget(name.to_string()));
                Outcome::Finished
            }
            PaletteLocalResult::OpenView(address) => {
                match address.parse::<flotilla_protocol::ViewAddress>() {
                    Ok(address) => ctx.app_actions.push(AppAction::OpenView(address)),
                    Err(e) => ctx.app_actions.push(AppAction::ShowStatus(e)),
                }
                Outcome::Finished
            }
        }
    }

    fn dispatch_resolved(&self, resolved: Resolved, ctx: &mut WidgetContext) -> Outcome {
        match tui_dispatch(resolved, ctx.model, ctx.provisioning_target) {
            Ok(mut command) => {
                if command.node_id.is_none() {
                    command.node_id.clone_from(&self.target_node_id);
                }
                ctx.commands.push(command);
            }
            Err(err) => {
                ctx.app_actions.push(AppAction::ShowStatus(err));
            }
        }
        Outcome::Finished
    }

    fn dispatch_palette_action(&self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        let interactions =
            crate::interaction::InteractionContext::for_active_view(ctx.views.active_address(), ctx.views.active_table_state().selected());
        if !interactions.is_available(action) {
            ctx.app_actions.push(AppAction::ShowStatus("That action is not available in this view".into()));
            return Outcome::Finished;
        }
        match action {
            // Actions that open other widgets — use Swap to replace the palette
            Action::OpenFind => {
                Outcome::Swap(Box::new(super::table_search::TableSearchWidget::find(&ctx.views.active_table_state().filter)))
            }
            Action::OpenFilePicker => {
                let start_dir = file_picker_start_dir();
                let input = Input::from(format!("{}/", start_dir.display()).as_str());
                let widget = super::file_picker::FilePickerWidget::open(input);
                Outcome::Swap(Box::new(widget))
            }
            Action::ToggleHelp => {
                let widget = super::help::HelpWidget::new();
                Outcome::Swap(Box::new(widget))
            }

            // Actions that map to AppActions — push the action and close the palette
            Action::Quit => {
                ctx.app_actions.push(AppAction::Quit);
                Outcome::Finished
            }
            Action::CycleTheme => {
                ctx.app_actions.push(AppAction::CycleTheme);
                Outcome::Finished
            }
            Action::CycleHost => {
                ctx.app_actions.push(AppAction::CycleHost);
                Outcome::Finished
            }
            Action::ToggleDebug => {
                ctx.app_actions.push(AppAction::ToggleDebug);
                Outcome::Finished
            }
            Action::ToggleStatusBarKeys => {
                ctx.app_actions.push(AppAction::ToggleStatusBarKeys);
                Outcome::Finished
            }
            Action::Refresh => {
                ctx.app_actions.push(AppAction::Refresh);
                Outcome::Finished
            }

            // Remaining actions that don't have meaningful palette behavior
            _ => Outcome::Finished,
        }
    }
}

/// Dispatch a resolved command with ambient context from the TUI environment.
pub(crate) fn tui_dispatch(resolved: Resolved, model: &TuiModel, provisioning_target: &ProvisioningTarget) -> Result<Command, String> {
    if !flotilla_commands::applicability::tui_actionable_resolved(&resolved) {
        return Err("Command has no TUI-visible effect".into());
    }
    match resolved {
        Resolved::HostQuery { .. } => Err("Command has no TUI-visible effect".into()),
        Resolved::Ready(cmd) => Ok(cmd),
        Resolved::NeedsContext { mut command, repo, host } => {
            if matches!(repo, RepoContext::Required | RepoContext::Inferred) {
                return Err("This command requires an explicit repository".into());
            }

            // Node resolution — only fill if not already set by explicit `host <name>` routing.
            // When the user types `host feta cr #42 open`, noun resolution sets command.node_id.
            if command.node_id.is_none() {
                match host {
                    HostResolution::Local => {}
                    HostResolution::ProvisioningTarget => {
                        let resolved_host = model.resolve_host(provisioning_target.host())?;
                        command.node_id = Some(resolved_host.summary.node.node_id.clone());
                        command.provisioning_target = Some(provisioning_target.clone());
                    }
                    HostResolution::Explicit(host) => {
                        let resolved_host = model.resolve_host(&host)?;
                        command.node_id = Some(resolved_host.summary.node.node_id.clone());
                        command.provisioning_target = Some(ProvisioningTarget::Host { host });
                    }
                    HostResolution::ExplicitEnvironment(environment_id) => {
                        let (node_id, target) = model.resolve_environment_target(&environment_id)?;
                        command.node_id = Some(node_id);
                        command.provisioning_target = Some(target);
                    }
                    HostResolution::SubjectHost | HostResolution::ProviderHost => {}
                }
            }

            Ok(command)
        }
    }
}

impl InteractiveWidget for CommandPaletteWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        let interactions =
            crate::interaction::InteractionContext::for_active_view(ctx.views.active_address(), ctx.views.active_table_state().selected());
        match action {
            Action::SelectNext => {
                let count = self.completions(ctx.model, ctx.namespaces, interactions).len();
                if count > 0 {
                    self.selected = (self.selected + 1) % count;
                    self.adjust_scroll();
                }
                Outcome::Consumed
            }
            Action::SelectPrev => {
                let count = self.completions(ctx.model, ctx.namespaces, interactions).len();
                if count > 0 {
                    self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
                    self.adjust_scroll();
                }
                Outcome::Consumed
            }
            Action::Confirm => self.confirm(ctx),
            Action::Dismiss => Outcome::Finished,
            Action::FillSelected => {
                let completions = self.completions(ctx.model, ctx.namespaces, interactions);
                if let Some(completion) = completions.get(self.selected) {
                    self.fill_completion(completion);
                    self.request_project_addresses(ctx);
                }
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        let interactions =
            crate::interaction::InteractionContext::for_active_view(ctx.views.active_address(), ctx.views.active_table_state().selected());
        // Right arrow: fill selected completion into input (Tab goes through handle_action)
        if matches!(key.code, KeyCode::Right) {
            let completions = self.completions(ctx.model, ctx.namespaces, interactions);
            if let Some(completion) = completions.get(self.selected) {
                self.fill_completion(completion);
            }
            return Outcome::Consumed;
        }

        // Backspace on empty input closes the palette
        if matches!(key.code, KeyCode::Backspace) && self.input.value().is_empty() {
            return Outcome::Finished;
        }

        self.input.handle_event(&crossterm::event::Event::Key(key));
        self.request_project_addresses(ctx);

        self.selected = 0;
        self.scroll_top = 0;
        Outcome::Consumed
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Outcome::Ignored;
        }
        let Some(overlay) = self.overlay else { return Outcome::Ignored };
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        if !overlay.body.contains(position) && !overlay.status_row.contains(position) {
            return Outcome::Finished;
        }
        if overlay.body.contains(position) {
            let interactions = crate::interaction::InteractionContext::for_active_view(
                ctx.views.active_address(),
                ctx.views.active_table_state().selected(),
            );
            let completions = self.completions(ctx.model, ctx.namespaces, interactions);
            let row = mouse.row - overlay.body.y;
            if row < self.hint_rows {
                return Outcome::Consumed;
            }
            let index = self.scroll_top + (row - self.hint_rows) as usize;
            if let Some(completion) = completions.get(index) {
                self.selected = index;
                self.fill_completion(completion);
                self.request_project_addresses(ctx);
            }
        }
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        let theme = ctx.theme;
        let interactions =
            crate::interaction::InteractionContext::for_active_view(ctx.views.active_address(), ctx.views.active_table_state().selected());
        let completions = self.completions(ctx.model, ctx.namespaces, interactions);
        let show_failure = palette::is_open_address_completion(self.input.value())
            && matches!(ctx.model.project_address_state, ProjectAddressState::Failed);
        let hint_rows = u16::from(show_failure);
        let overlay = crate::ui_helpers::bottom_anchored_overlay(frame.area(), 1, MAX_PALETTE_ROWS as u16 + hint_rows);
        self.hint_rows = hint_rows.min(overlay.visible_body_rows);
        self.completion_rows = overlay.visible_body_rows.saturating_sub(self.hint_rows) as usize;
        self.adjust_scroll();
        self.overlay = Some(overlay);
        let area = overlay.body;

        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

        if self.hint_rows > 0 {
            frame.render_widget(
                Paragraph::new(" Projects unavailable; reopen to retry").style(Style::default().fg(theme.muted)),
                Rect::new(area.x, area.y, area.width, 1),
            );
        }

        let showing_addresses = palette::is_open_address_completion(self.input.value());
        let name_width =
            completions.iter().map(|completion| completion_display_text(completion, showing_addresses).width()).max().unwrap_or(0).min(20);
        let hint_width: u16 = 7;

        for (i, completion) in completions.iter().skip(self.scroll_top).take(self.completion_rows).enumerate() {
            let row_y = area.y + self.hint_rows + i as u16;
            let is_selected = self.scroll_top + i == self.selected;

            let row_style = if is_selected {
                Style::default().bg(theme.action_highlight).add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(theme.bar_bg)
            };

            let row_area = Rect::new(area.x, row_y, area.width, 1);
            frame.render_widget(Block::default().style(row_style), row_area);

            let name_span = Span::styled(
                format!("  {:<width$}", completion_display_text(completion, showing_addresses), width = name_width),
                row_style.fg(theme.text),
            );
            let desc_span = Span::styled(
                if showing_addresses { String::new() } else { format!("  {}", completion.description) },
                row_style.fg(theme.muted),
            );

            let line = Line::from(vec![name_span, desc_span]);
            frame.render_widget(Paragraph::new(line), Rect::new(area.x, row_y, area.width.saturating_sub(hint_width), 1));

            let hint_text = completion.key_hint.unwrap_or("");
            if !hint_text.is_empty() {
                let hint_span = Span::styled(format!(" {} ", hint_text), row_style.fg(theme.key_hint));
                let hint_x = area.x + area.width.saturating_sub(hint_width);
                frame.render_widget(Paragraph::new(Line::from(hint_span)), Rect::new(hint_x, row_y, hint_width, 1));
            }
        }

        // Cursor on the status bar row (computed via the same overlay layout)
        let cursor_x = overlay.status_row.x + 1 + self.input.visual_cursor() as u16;
        let indicator = match palette::palette_input_state(self.input.value()) {
            PaletteInputState::Ready => "ready",
            PaletteInputState::Incomplete => "incomplete",
            PaletteInputState::Unavailable => "CLI only",
        };
        let input_end_x = overlay.status_row.x.saturating_add(1).saturating_add(self.input.value().width() as u16);
        let indicator_x = input_end_x.saturating_add(2);
        if indicator_x.saturating_add(indicator.len() as u16) <= overlay.status_row.right() {
            frame.render_widget(
                Paragraph::new(indicator).style(Style::default().fg(theme.muted).bg(theme.bar_bg)),
                Rect::new(indicator_x, overlay.status_row.y, indicator.len() as u16, 1),
            );
        }
        frame.set_cursor_position((cursor_x, overlay.status_row.y));
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::CommandPalette.into()
    }

    fn captures_raw_keys(&self) -> bool {
        false
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::ActiveInput { prefix: ":".into(), text: self.input.value().to_string() }) }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use flotilla_protocol::CommandAction;

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    #[test]
    fn typed_cli_query_cannot_bypass_palette_dispatch_gate() {
        let harness = TestWidgetHarness::new();
        let command = Command::builder().action(CommandAction::QueryFleetReplicaSnapshot {}).build();
        let result = tui_dispatch(Resolved::Ready(command), &harness.model, &harness.provisioning_target);
        assert!(result.is_err());
    }
    fn render_for_mouse(widget: &mut CommandPaletteWidget, harness: &mut TestWidgetHarness) {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("test terminal");
        let mut ui = crate::app::UiState::new(&[]);
        let theme = crate::theme::Theme::classic();
        terminal
            .draw(|frame| {
                let mut ctx = RenderContext {
                    model: &harness.model,
                    views: &mut harness.views,
                    ui: &mut ui,
                    theme: &theme,
                    keymap: &harness.keymap,
                    in_flight: &harness.in_flight,
                    namespaces: &harness.namespaces,
                    query_tables: &harness.query_tables,
                };
                widget.render(frame, frame.area(), &mut ctx);
            })
            .expect("render palette");
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: crossterm::event::KeyModifiers::NONE }
    }

    #[test]
    fn refresh_uses_active_project_and_checkout_queries_without_repository_context() {
        for address in ["project/flotilla/roadmap", "checkouts?project=flotilla%2Froadmap"] {
            let mut app = crate::app::test_support::stub_app();
            app.views.open_or_focus(address.parse().expect("view address"));
            app.sync_active_view();
            app.subscriptions_dirty = false;
            let mut widget = CommandPaletteWidget::with_state(Input::from("refresh"), 0, 0);
            let mut ctx = app.build_widget_context();
            assert!(matches!(widget.handle_action(Action::Confirm, &mut ctx), Outcome::Finished));
            let actions = std::mem::take(&mut ctx.app_actions);
            assert!(actions.iter().any(|action| matches!(action, AppAction::Refresh)), "{address}: {actions:?}");
            drop(ctx);
            app.process_app_actions(actions);
            assert!(app.subscriptions_dirty, "refresh should invalidate active query subscriptions");
            assert!(app.proto_commands.take_next().is_none(), "refresh uses queries rather than a repository command");
        }
    }

    #[test]
    fn binding_mode_is_command_palette() {
        let widget = CommandPaletteWidget::new();
        assert_eq!(widget.binding_mode(), KeyBindingMode::from(BindingModeId::CommandPalette));
    }

    #[test]
    fn command_palette_routes_text_through_raw_key_handling() {
        let widget = CommandPaletteWidget::new();
        assert!(!widget.captures_raw_keys());
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::Dismiss, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let interactions = crate::interaction::InteractionContext::for_active_view(
            harness.views.active_address(),
            harness.views.active_table_state().selected(),
        );
        let count = widget.completions(&harness.model, &harness.namespaces, interactions).len();
        assert!(count > 1);

        widget.handle_action(Action::SelectPrev, &mut harness.ctx());
        assert_eq!(widget.selected, count - 1);
        widget.handle_action(Action::SelectNext, &mut harness.ctx());
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn incomplete_input_keeps_palette_open_without_dispatch() {
        let mut widget = CommandPaletteWidget::with_state(Input::from("cr"), 0, 0);
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::Confirm, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn engaging_open_completion_requests_project_addresses_lazily() {
        let mut widget = CommandPaletteWidget::with_state(Input::from("open"), 0, 0);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();
        widget.handle_raw_key(KeyEvent::new(KeyCode::Char(' '), crossterm::event::KeyModifiers::NONE), &mut ctx);
        assert!(ctx.app_actions.iter().any(|action| matches!(action, AppAction::LoadProjectAddresses)));
    }

    #[test]
    fn failed_project_load_retries_only_in_a_new_palette() {
        let mut widget = CommandPaletteWidget::with_state(Input::from("open"), 0, 0);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();
        widget.handle_raw_key(KeyEvent::new(KeyCode::Char(' '), crossterm::event::KeyModifiers::NONE), &mut ctx);
        assert_eq!(ctx.app_actions.len(), 1);
        drop(ctx);

        harness.model.project_address_state = crate::app::ProjectAddressState::Failed;
        let mut ctx = harness.ctx();
        widget.handle_raw_key(KeyEvent::new(KeyCode::Char('p'), crossterm::event::KeyModifiers::NONE), &mut ctx);
        assert!(ctx.app_actions.is_empty(), "the same palette does not retry on each keypress");
        drop(ctx);

        let mut reopened = CommandPaletteWidget::with_state(Input::from("open"), 0, 0);
        let mut ctx = harness.ctx();
        reopened.handle_raw_key(KeyEvent::new(KeyCode::Char(' '), crossterm::event::KeyModifiers::NONE), &mut ctx);
        assert!(ctx.app_actions.iter().any(|action| matches!(action, AppAction::LoadProjectAddresses)));
    }

    #[test]
    fn a_new_palette_refreshes_cached_project_addresses() {
        let mut harness = TestWidgetHarness::new();
        harness.model.project_address_state =
            ProjectAddressState::Loaded(vec!["project/flotilla/roadmap".parse().expect("project address")]);
        let mut widget = CommandPaletteWidget::with_state(Input::from("open"), 0, 0);
        let mut ctx = harness.ctx();
        widget.handle_raw_key(KeyEvent::new(KeyCode::Char(' '), crossterm::event::KeyModifiers::NONE), &mut ctx);
        assert!(ctx.app_actions.iter().any(|action| matches!(action, AppAction::LoadProjectAddresses)));
    }

    #[test]
    fn failed_project_completions_show_persistent_hint_and_keep_ambient_rows() {
        let mut harness = TestWidgetHarness::new();
        harness.model.project_address_state = ProjectAddressState::Failed;
        let mut widget = CommandPaletteWidget::with_state(Input::from("open "), 0, 0);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("test terminal");
        let mut ui = crate::app::UiState::new(&[]);
        let theme = crate::theme::Theme::classic();
        terminal
            .draw(|frame| {
                let mut ctx = RenderContext {
                    model: &harness.model,
                    views: &mut harness.views,
                    ui: &mut ui,
                    theme: &theme,
                    keymap: &harness.keymap,
                    in_flight: &harness.in_flight,
                    namespaces: &harness.namespaces,
                    query_tables: &harness.query_tables,
                };
                widget.render(frame, frame.area(), &mut ctx);
            })
            .expect("render palette");
        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("Projects unavailable; reopen to retry"));
        assert!(rendered.contains("overview"));
    }

    #[test]
    fn open_address_rows_render_decoded_human_labels() {
        let mut harness = TestWidgetHarness::new();
        harness.model.project_address_state =
            ProjectAddressState::Loaded(vec!["project/flotilla/r%C3%A9sum%C3%A9".parse().expect("project address")]);
        let mut widget = CommandPaletteWidget::with_state(Input::from("open project/"), 0, 0);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("test terminal");
        let mut ui = crate::app::UiState::new(&[]);
        let theme = crate::theme::Theme::classic();
        terminal
            .draw(|frame| {
                let mut ctx = RenderContext {
                    model: &harness.model,
                    views: &mut harness.views,
                    ui: &mut ui,
                    theme: &theme,
                    keymap: &harness.keymap,
                    in_flight: &harness.in_flight,
                    namespaces: &harness.namespaces,
                    query_tables: &harness.query_tables,
                };
                widget.render(frame, frame.area(), &mut ctx);
            })
            .expect("render palette");
        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("project/flotilla/résumé"));
        assert!(!rendered.contains("r%C3%A9sum%C3%A9"));
    }

    #[test]
    fn selecting_open_completion_dispatches_canonical_address() {
        let mut harness = TestWidgetHarness::new();
        harness.model.project_address_state =
            ProjectAddressState::Loaded(vec!["project/flotilla/road%20map".parse().expect("project address")]);
        let mut widget = CommandPaletteWidget::with_state(Input::from("open road"), 0, 0);
        let mut ctx = harness.ctx();
        assert!(matches!(widget.handle_action(Action::FillSelected, &mut ctx), Outcome::Consumed));
        assert_eq!(widget.input_value(), "open project/flotilla/road%20map ");
        assert!(matches!(widget.handle_action(Action::Confirm, &mut ctx), Outcome::Finished));
        assert!(ctx
            .app_actions
            .iter()
            .any(|action| matches!(action, AppAction::OpenView(address) if address.to_string() == "project/flotilla/road%20map")));
    }

    #[test]
    fn clicking_outside_palette_dismisses_it() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        render_for_mouse(&mut widget, &mut harness);
        let outcome = widget.handle_mouse(left_click(5, 0), &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn clicking_completion_fills_that_suggestion() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        render_for_mouse(&mut widget, &mut harness);
        let interactions = crate::interaction::InteractionContext::for_active_view(
            harness.views.active_address(),
            harness.views.active_table_state().selected(),
        );
        let expected = widget.completions(&harness.model, &harness.namespaces, interactions)[1].value.clone();
        let body = widget.overlay.expect("rendered overlay").body;
        let outcome = widget.handle_mouse(left_click(body.x + 2, body.y + 1), &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input_value(), format!("{expected} "));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn clicking_below_last_completion_keeps_input() {
        let mut widget = CommandPaletteWidget::with_state(Input::from("theme cat"), 0, 0);
        let mut harness = TestWidgetHarness::new();
        render_for_mouse(&mut widget, &mut harness);
        let body = widget.overlay.expect("rendered overlay").body;
        let outcome = widget.handle_mouse(left_click(body.x + 2, body.y + 3), &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input_value(), "theme cat");
    }

    #[test]
    fn clicking_scrolled_completion_fills_visible_row() {
        let mut widget = CommandPaletteWidget::with_state(Input::from(""), 2, 2);
        let mut harness = TestWidgetHarness::new();
        render_for_mouse(&mut widget, &mut harness);
        let interactions = crate::interaction::InteractionContext::for_active_view(
            harness.views.active_address(),
            harness.views.active_table_state().selected(),
        );
        let expected = widget.completions(&harness.model, &harness.namespaces, interactions)[2].value.clone();
        let body = widget.overlay.expect("rendered overlay").body;
        let outcome = widget.handle_mouse(left_click(body.x + 2, body.y), &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input_value(), format!("{expected} "));
    }

    #[test]
    fn readiness_indicator_preserves_input_when_cursor_moves_left() {
        let mut widget = CommandPaletteWidget::with_state(Input::from("refresh"), 0, 0);
        let mut harness = TestWidgetHarness::new();
        for _ in 0..4 {
            widget.handle_raw_key(KeyEvent::new(KeyCode::Left, crossterm::event::KeyModifiers::NONE), &mut harness.ctx());
        }
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("test terminal");
        let mut ui = crate::app::UiState::new(&[]);
        let theme = crate::theme::Theme::classic();
        terminal
            .draw(|frame| {
                let status_row = crate::ui_helpers::bottom_anchored_overlay(frame.area(), 1, MAX_PALETTE_ROWS as u16).status_row;
                frame.render_widget(Paragraph::new(":refresh"), status_row);
                let mut ctx = RenderContext {
                    model: &harness.model,
                    views: &mut harness.views,
                    ui: &mut ui,
                    theme: &theme,
                    keymap: &harness.keymap,
                    in_flight: &harness.in_flight,
                    namespaces: &harness.namespaces,
                    query_tables: &harness.query_tables,
                };
                widget.render(frame, frame.area(), &mut ctx);
            })
            .expect("render palette");
        let status_row = widget.overlay.expect("rendered overlay").status_row;
        let buffer = terminal.backend().buffer();
        let input: String = (0..8).map(|x| buffer[(x, status_row.y)].symbol()).collect();
        assert_eq!(input, ":refresh");
    }
}
