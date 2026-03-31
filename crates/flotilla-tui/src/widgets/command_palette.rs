use std::any::Any;

use crossterm::event::{KeyCode, KeyEvent};
use flotilla_commands::{HostResolution, RepoContext, Resolved};
use flotilla_protocol::{Command, CommandAction, HostName, ProvisioningTarget, RepoIdentity, RepoSelector, WorkItem};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::TuiModel,
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    palette::{self, PaletteCompletion, PaletteEntry, PaletteLocalResult, PaletteParseResult, MAX_PALETTE_ROWS},
};

/// Map a work item to a palette pre-fill string.
pub fn palette_prefill(item: &WorkItem) -> Option<String> {
    if let Some(cr_key) = &item.change_request_key {
        return Some(format!("cr {} ", cr_key));
    }
    if let Some(branch) = &item.branch {
        if item.checkout_key().is_some() {
            return Some(format!("checkout {} ", branch));
        }
    }
    if let Some(issue_key) = item.issue_keys.first() {
        return Some(format!("issue {} ", issue_key));
    }
    if let Some(session_key) = &item.session_key {
        return Some(format!("agent {} ", session_key));
    }
    if let Some(ws_ref) = item.workspace_refs.first() {
        return Some(format!("workspace {} ", ws_ref));
    }
    None
}

pub struct CommandPaletteWidget {
    input: Input,
    entries: &'static [PaletteEntry],
    selected: usize,
    scroll_top: usize,
    /// The work item that was selected when the contextual palette was opened.
    /// Used by tui_dispatch for SubjectHost/ProviderHost resolution.
    source_item: Option<WorkItem>,
}

impl Default for CommandPaletteWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPaletteWidget {
    pub fn new() -> Self {
        Self { input: Input::default(), entries: palette::all_entries(), selected: 0, scroll_top: 0, source_item: None }
    }

    /// Create a palette widget with pre-filled input text and selection.
    pub fn with_state(input: Input, selected: usize, scroll_top: usize) -> Self {
        Self { input, entries: palette::all_entries(), selected, scroll_top, source_item: None }
    }

    /// Create a palette widget with a pre-filled input string (cursor at end)
    /// and the work item that was selected when the palette was opened.
    pub fn with_prefill(text: impl AsRef<str>, item: Option<WorkItem>) -> Self {
        Self { input: Input::from(text.as_ref()), entries: palette::all_entries(), selected: 0, scroll_top: 0, source_item: item }
    }

    fn filtered(&self) -> Vec<&'static PaletteEntry> {
        palette::filter_entries(self.entries, self.input.value())
    }

    /// Compute position-aware completions using model context.
    fn completions(&self, model: &TuiModel, has_repo_context: bool) -> Vec<PaletteCompletion> {
        palette::palette_completions(self.input.value(), model, has_repo_context)
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
        let max_visible = MAX_PALETTE_ROWS;
        if self.selected >= self.scroll_top + max_visible {
            self.scroll_top = self.selected.saturating_sub(max_visible - 1);
        } else if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        }
    }

    fn confirm(&mut self, ctx: &mut WidgetContext) -> Outcome {
        let text = self.input.value().to_string();

        match palette::parse_palette_input(&text) {
            Ok(PaletteParseResult::Local(local)) => self.dispatch_local(local, ctx),
            Ok(PaletteParseResult::Resolved(resolved)) => self.dispatch_resolved(resolved, ctx),
            Err(err) => {
                // If parse failed, fall back to the selected entry's action (fuzzy match)
                let filtered = self.filtered();
                if let Some(entry) = filtered.get(self.selected) {
                    let action = entry.action;
                    return self.dispatch_palette_action(action, ctx);
                }
                ctx.app_actions.push(AppAction::ShowStatus(err));
                Outcome::Finished
            }
        }
    }

    fn dispatch_local(&mut self, local: PaletteLocalResult<'_>, ctx: &mut WidgetContext) -> Outcome {
        match local {
            PaletteLocalResult::Action(action) => self.dispatch_palette_action(action, ctx),
            PaletteLocalResult::SetLayout(name) => {
                ctx.app_actions.push(AppAction::SetLayout(name.to_string()));
                Outcome::Finished
            }
            PaletteLocalResult::SetTheme(name) => {
                ctx.app_actions.push(AppAction::SetTheme(name.to_string()));
                Outcome::Finished
            }
            PaletteLocalResult::SetTarget(name) => {
                ctx.app_actions.push(AppAction::SetTarget(name.to_string()));
                Outcome::Finished
            }
            PaletteLocalResult::Search(query) => {
                if *ctx.is_config {
                    ctx.app_actions.push(AppAction::ShowStatus("switch to a repo tab first".into()));
                    return Outcome::Finished;
                }
                let query = query.trim().to_string();
                let Some(repo_identity) = ctx.model.active_repo_identity_opt().cloned() else {
                    ctx.app_actions.push(AppAction::ShowStatus("No active repo".into()));
                    return Outcome::Finished;
                };
                if query.is_empty() {
                    ctx.app_actions.push(AppAction::ClearSearchQuery { repo: repo_identity });
                } else {
                    let cmd = Command {
                        host: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::QueryIssues {
                            repo: RepoSelector::Identity(repo_identity.clone()),
                            params: flotilla_protocol::issue_query::IssueQuery { search: Some(query.clone()) },
                            page: 1,
                            count: 50,
                        },
                    };
                    ctx.commands.push(cmd);
                    ctx.app_actions.push(AppAction::SetSearchQuery { repo: repo_identity, query });
                }
                Outcome::Finished
            }
        }
    }

    fn dispatch_resolved(&self, resolved: Resolved, ctx: &mut WidgetContext) -> Outcome {
        let active_repo = ctx.model.active_repo_identity_opt().cloned();
        match tui_dispatch(
            resolved,
            self.source_item.as_ref(),
            *ctx.is_config,
            active_repo.as_ref(),
            ctx.provisioning_target,
            &ctx.my_host,
            ctx.active_repo_is_remote_only,
        ) {
            Ok(command) => {
                ctx.commands.push(command);
            }
            Err(err) => {
                ctx.app_actions.push(AppAction::ShowStatus(err));
            }
        }
        Outcome::Finished
    }

    fn dispatch_palette_action(&self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            // Actions that open other widgets — use Swap to replace the palette
            Action::OpenBranchInput => {
                if *ctx.is_config || ctx.model.active_repo_identity_opt().is_none() {
                    ctx.app_actions.push(AppAction::ShowStatus("switch to a repo tab first".into()));
                    return Outcome::Finished;
                }
                let widget = super::branch_input::BranchInputWidget::new(crate::app::ui_state::BranchInputKind::Manual);
                Outcome::Swap(Box::new(widget))
            }
            Action::OpenIssueSearch => {
                if *ctx.is_config || ctx.model.active_repo_identity_opt().is_none() {
                    ctx.app_actions.push(AppAction::ShowStatus("switch to a repo tab first".into()));
                    Outcome::Finished
                } else {
                    Outcome::Swap(Box::new(super::issue_search::IssueSearchWidget::new()))
                }
            }
            Action::OpenFilePicker => {
                let start_dir = ctx
                    .model
                    .active_repo_root_opt()
                    .and_then(|r| r.parent())
                    .map(|p| p.to_path_buf())
                    .or_else(|| std::env::current_dir().ok())
                    .or_else(dirs::home_dir)
                    .unwrap_or_default();
                let input = Input::from(format!("{}/", start_dir.display()).as_str());
                let dir_entries = refresh_dir_listing_standalone(input.value(), ctx.model);
                let widget = super::file_picker::FilePickerWidget::new(input.clone(), dir_entries);
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
            Action::CycleLayout => {
                ctx.app_actions.push(AppAction::CycleLayout);
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
                if *ctx.is_config {
                    ctx.app_actions.push(AppAction::ShowStatus("switch to a repo tab first".into()));
                    return Outcome::Finished;
                }
                ctx.app_actions.push(AppAction::Refresh);
                Outcome::Finished
            }

            // Widget-level actions dispatched via AppAction
            Action::ToggleProviders => {
                ctx.app_actions.push(AppAction::ToggleProviders);
                Outcome::Finished
            }
            Action::ToggleMultiSelect => {
                ctx.app_actions.push(AppAction::ToggleMultiSelect);
                Outcome::Finished
            }
            Action::OpenActionMenu => {
                if *ctx.is_config {
                    ctx.app_actions.push(AppAction::ShowStatus("switch to a repo tab first".into()));
                    return Outcome::Finished;
                }
                ctx.app_actions.push(AppAction::OpenActionMenu);
                Outcome::Finished
            }

            // Remaining actions that don't have meaningful palette behavior
            _ => Outcome::Finished,
        }
    }
}

/// Standalone directory listing that doesn't require `&mut App`.
pub fn refresh_dir_listing_standalone(path_str: &str, model: &crate::app::TuiModel) -> Vec<crate::app::ui_state::DirEntry> {
    use std::path::PathBuf;

    use crate::app::ui_state::DirEntry;

    let dir = if path_str.ends_with('/') {
        PathBuf::from(path_str)
    } else {
        PathBuf::from(path_str).parent().map(|p| p.to_path_buf()).unwrap_or_default()
    };

    let filter = if !path_str.ends_with('/') {
        PathBuf::from(path_str).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
    } else {
        String::new()
    };

    let mut entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if !filter.is_empty() && !name.to_lowercase().starts_with(&filter) {
                continue;
            }
            let path = entry.path();
            let is_dir = path.is_dir();
            if !is_dir {
                continue;
            }
            let is_git_repo = path.join(".git").exists();
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            let is_added = model.repos.values().any(|repo| repo.path == canonical);
            entries.push(DirEntry { name, is_dir, is_git_repo, is_added });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Fill SENTINEL empty `RepoSelector::Query("")` fields in a `CommandAction` with a real repo selector.
fn fill_repo_sentinels(action: &mut CommandAction, repo: RepoSelector) {
    match action {
        CommandAction::Checkout { repo: r, .. } if *r == RepoSelector::Query(String::new()) => *r = repo,
        CommandAction::QueryIssues { repo: r, .. } if *r == RepoSelector::Query(String::new()) => *r = repo,
        _ => {}
    }
}

/// Resolve the host a work item should execute on relative to our own host.
fn item_execution_host(item: &WorkItem, my_host: &Option<HostName>) -> Option<HostName> {
    match my_host {
        Some(host) if item.host != *host => Some(item.host.clone()),
        _ => None,
    }
}

/// Dispatch a resolved command with ambient context from the TUI environment.
pub(crate) fn tui_dispatch(
    resolved: Resolved,
    item: Option<&WorkItem>,
    is_config: bool,
    active_repo: Option<&RepoIdentity>,
    provisioning_target: &ProvisioningTarget,
    my_host: &Option<HostName>,
    active_repo_is_remote_only: bool,
) -> Result<Command, String> {
    match resolved {
        Resolved::Ready(cmd) => Ok(cmd),
        Resolved::NeedsContext { mut command, repo, host } => {
            // Repo context from active tab
            let tab_repo = if is_config {
                None // overview tab — no repo context
            } else {
                active_repo.map(|id| RepoSelector::Identity(id.clone()))
            };

            match repo {
                RepoContext::Required => {
                    let repo_sel = tab_repo.ok_or_else(|| "no active repo — switch to a repo tab first".to_string())?;
                    command.context_repo = Some(repo_sel.clone());
                    fill_repo_sentinels(&mut command.action, repo_sel);
                }
                RepoContext::Inferred => {
                    if is_config {
                        return Err("no active repo — switch to a repo tab first".to_string());
                    }
                    command.context_repo = tab_repo;
                }
            }

            // Host resolution — only fill if not already set by explicit `host <name>` routing.
            // When the user types `host feta cr #42 open`, HostNoun::resolve() calls set_host("feta")
            // during noun resolution, so command.host is already Some. We must not clobber it.
            if command.host.is_none() {
                match host {
                    HostResolution::Local => {}
                    HostResolution::ProvisioningTarget => {
                        command.host = Some(provisioning_target.host().clone());
                        command.provisioning_target = Some(provisioning_target.clone());
                    }
                    HostResolution::SubjectHost => {
                        command.host = item.and_then(|i| item_execution_host(i, my_host));
                    }
                    HostResolution::ProviderHost => {
                        if active_repo_is_remote_only {
                            command.host = item.and_then(|i| item_execution_host(i, my_host));
                        }
                    }
                }
            }

            Ok(command)
        }
    }
}

impl InteractiveWidget for CommandPaletteWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        let has_repo_context = !*ctx.is_config;
        match action {
            Action::SelectNext => {
                let count = self.completions(ctx.model, has_repo_context).len();
                if count > 0 {
                    self.selected = (self.selected + 1) % count;
                    self.adjust_scroll();
                }
                Outcome::Consumed
            }
            Action::SelectPrev => {
                let count = self.completions(ctx.model, has_repo_context).len();
                if count > 0 {
                    self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
                    self.adjust_scroll();
                }
                Outcome::Consumed
            }
            Action::Confirm => self.confirm(ctx),
            Action::Dismiss => Outcome::Finished,
            Action::FillSelected => {
                let completions = self.completions(ctx.model, has_repo_context);
                if let Some(completion) = completions.get(self.selected) {
                    self.fill_completion(completion);
                }
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        let has_repo_context = !*ctx.is_config;
        // Right arrow: fill selected completion into input (Tab goes through handle_action)
        if matches!(key.code, KeyCode::Right) {
            let completions = self.completions(ctx.model, has_repo_context);
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

        // Shortcut: typing / when input is empty fills "search "
        if self.input.value() == "/" {
            self.input = Input::from("search ");
            self.selected = 0;
            self.scroll_top = 0;
            return Outcome::Consumed;
        }

        self.selected = 0;
        self.scroll_top = 0;
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        let theme = ctx.theme;
        let has_repo_context = !ctx.ui.is_config;
        let completions = self.completions(ctx.model, has_repo_context);
        let overlay = crate::ui_helpers::bottom_anchored_overlay(frame.area(), 1, MAX_PALETTE_ROWS as u16);
        let area = overlay.body;

        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

        let name_width = completions.iter().map(|c| c.value.len()).max().unwrap_or(0).min(20);
        let hint_width: u16 = 7;

        for (i, completion) in completions.iter().skip(self.scroll_top).take(overlay.visible_body_rows as usize).enumerate() {
            let row_y = area.y + i as u16;
            let is_selected = self.scroll_top + i == self.selected;

            let row_style = if is_selected {
                Style::default().bg(theme.action_highlight).add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(theme.bar_bg)
            };

            let row_area = Rect::new(area.x, row_y, area.width, 1);
            frame.render_widget(Block::default().style(row_style), row_area);

            let name_span = Span::styled(format!("  {:<width$}", completion.value, width = name_width), row_style.fg(theme.text));
            let desc_span = Span::styled(format!("  {}", completion.description), row_style.fg(theme.muted));

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
        frame.set_cursor_position((cursor_x, overlay.status_row.y));
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::CommandPalette.into()
    }

    fn captures_raw_keys(&self) -> bool {
        false
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::ActiveInput { prefix: "/".into(), text: self.input.value().to_string() }) }
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use flotilla_protocol::{Command, CommandAction, ProvisioningTarget, WorkItemIdentity, WorkItemKind};

    use super::*;
    use crate::app::test_support::{bare_item, checkout_item, session_item, TestWidgetHarness};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn binding_mode_is_command_palette() {
        let widget = CommandPaletteWidget::new();
        assert_eq!(widget.binding_mode(), KeyBindingMode::from(BindingModeId::CommandPalette));
    }

    #[test]
    fn does_not_capture_raw_keys() {
        let widget = CommandPaletteWidget::new();
        assert!(!widget.captures_raw_keys());
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();
        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn select_next_wraps_around() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let total = widget.completions(&harness.model, true).len();

        // Advance to end
        for _ in 0..total - 1 {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx);
        }
        assert_eq!(widget.selected, total - 1);

        // One more wraps to 0
        let mut ctx = harness.ctx();
        widget.handle_action(Action::SelectNext, &mut ctx);
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn select_prev_wraps_around() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let total = widget.completions(&harness.model, true).len();

        // Prev from 0 wraps to end
        let mut ctx = harness.ctx();
        widget.handle_action(Action::SelectPrev, &mut ctx);
        assert_eq!(widget.selected, total - 1);
    }

    #[test]
    fn fill_selected_fills_completion_value() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();

        // Get the first completion value to verify fill works
        let first_value = widget.completions(&harness.model, true)[0].value.clone();

        let mut ctx = harness.ctx();
        let outcome = widget.handle_action(Action::FillSelected, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), format!("{first_value} "));
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn backspace_on_empty_closes_palette() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_raw_key(key(KeyCode::Backspace), &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn slash_fills_search_prefix() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_raw_key(key(KeyCode::Char('/')), &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "search ");
    }

    #[test]
    fn confirm_search_pushes_search_command() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("search bug fix");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected QueryIssues command");
        match cmd {
            Command { action: CommandAction::QueryIssues { params, .. }, .. } => {
                assert_eq!(params.search.as_deref(), Some("bug fix"));
            }
            other => panic!("expected QueryIssues, got {:?}", other),
        }
    }

    #[test]
    fn confirm_search_empty_clears() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("search ");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        // Empty search no longer sends a command — only a ClearSearchQuery app action
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::ClearSearchQuery { .. })));
        drop(ctx);
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_entry_quit_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        // Type "quit" to filter to the quit entry
        widget.input = Input::from("quit");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    #[test]
    fn confirm_entry_branch_returns_swap() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("branch");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Swap(_)));
    }

    #[test]
    fn confirm_entry_search_returns_swap() {
        let mut widget = CommandPaletteWidget::new();
        // "search" as entry name (not "search <terms>")
        widget.input = Input::from("search");
        // Make sure selected is 0 so it picks "search" entry
        widget.selected = 0;
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        // "search" entry has action OpenIssueSearch, which should Swap
        assert!(matches!(outcome, Outcome::Swap(_)));
    }

    #[test]
    fn confirm_entry_help_returns_swap() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("help");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Swap(_)));
    }

    #[test]
    fn typing_text_resets_selected() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();

        // Move selected down
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx);
        }
        assert_eq!(widget.selected, 1);

        // Type a character — selected should reset to 0
        let mut ctx = harness.ctx();
        widget.handle_raw_key(key(KeyCode::Char('r')), &mut ctx);
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn confirm_entry_providers_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("providers");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::ToggleProviders)));
    }

    #[test]
    fn confirm_entry_select_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("select");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::ToggleMultiSelect)));
    }

    #[test]
    fn confirm_entry_actions_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("actions");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::OpenActionMenu)));
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::PrevTab, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn confirm_noun_verb_pushes_command() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("cr 42 close");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        assert!(matches!(cmd.action, CommandAction::CloseChangeRequest { ref id } if id == "42"));
    }

    #[test]
    fn confirm_noun_verb_required_repo_on_overview_tab_rejects() {
        // `checkout create --branch feat` uses RepoContext::Required — must fail on overview tab
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("checkout create --branch feat");
        let mut harness = TestWidgetHarness::new();
        harness.is_config = true;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::ShowStatus(msg) if msg.contains("repo tab"))));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_noun_verb_inferred_repo_on_overview_tab_rejects() {
        // `cr 42 close` uses RepoContext::Inferred — should be rejected on overview tab
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("cr 42 close");
        let mut harness = TestWidgetHarness::new();
        harness.is_config = true;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::ShowStatus(msg) if msg.contains("repo tab"))));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_layout_set_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("layout zoom");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::SetLayout(name) if name == "zoom")));
    }

    #[test]
    fn confirm_theme_set_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("theme catppuccin-mocha");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::SetTheme(name) if name == "catppuccin-mocha")));
    }

    #[test]
    fn confirm_target_set_pushes_app_action() {
        let mut widget = CommandPaletteWidget::new();
        widget.input = Input::from("target feta");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::SetTarget(name) if name == "feta")));
    }

    // ── palette_prefill ──

    #[test]
    fn prefill_from_cr() {
        let mut item = bare_item();
        item.change_request_key = Some("42".into());
        assert_eq!(palette_prefill(&item), Some("cr 42 ".into()));
    }

    #[test]
    fn prefill_prefers_cr_over_checkout() {
        let mut item = checkout_item("feat", "/tmp/repo", false);
        item.change_request_key = Some("42".into());
        assert_eq!(palette_prefill(&item), Some("cr 42 ".into()));
    }

    #[test]
    fn prefill_empty_item_returns_none() {
        let item = bare_item();
        assert_eq!(palette_prefill(&item), None);
    }

    #[test]
    fn prefill_from_checkout_branch() {
        let item = checkout_item("feat/my-branch", "/tmp/repo", false);
        assert_eq!(palette_prefill(&item), Some("checkout feat/my-branch ".into()));
    }

    #[test]
    fn prefill_from_session_key() {
        let item = session_item("ses-123");
        assert_eq!(palette_prefill(&item), Some("agent ses-123 ".into()));
    }

    #[test]
    fn prefill_from_issue_key() {
        let mut item = bare_item();
        item.issue_keys = vec!["99".into()];
        assert_eq!(palette_prefill(&item), Some("issue 99 ".into()));
    }

    #[test]
    fn prefill_from_workspace_ref() {
        let mut item = bare_item();
        item.workspace_refs = vec!["ws-abc".into()];
        assert_eq!(palette_prefill(&item), Some("workspace ws-abc ".into()));
    }

    // ── tui_dispatch ──

    #[test]
    fn dispatch_ready_passes_through() {
        let cmd = Command { host: None, provisioning_target: None, context_repo: None, action: CommandAction::Refresh { repo: None } };
        let local_target = ProvisioningTarget::Host { host: HostName::local() };
        let result = tui_dispatch(Resolved::Ready(cmd), None, false, None, &local_target, &None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn dispatch_needs_repo_on_overview_errors() {
        use flotilla_protocol::CheckoutTarget;
        let cmd = Command {
            host: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Query("".into()),
                target: CheckoutTarget::Branch("feat".into()),
                issue_ids: vec![],
            },
        };
        let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Required, host: HostResolution::ProvisioningTarget };
        let local_target = ProvisioningTarget::Host { host: HostName::local() };
        let result = tui_dispatch(resolved, None, true, None, &local_target, &None, false);
        assert!(result.is_err());
    }

    #[test]
    fn dispatch_fills_repo_sentinels() {
        use flotilla_protocol::CheckoutTarget;
        let cmd = Command {
            host: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::Checkout {
                repo: RepoSelector::Query("".into()),
                target: CheckoutTarget::Branch("feat".into()),
                issue_ids: vec![],
            },
        };
        let repo_id = RepoIdentity { authority: "github.com".into(), path: "org/repo".into() };
        let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Required, host: HostResolution::Local };
        let local_target = ProvisioningTarget::Host { host: HostName::local() };
        let result = tui_dispatch(resolved, None, false, Some(&repo_id), &local_target, &None, false).unwrap();
        assert!(result.context_repo.is_some());
        match &result.action {
            CommandAction::Checkout { repo, .. } => assert_ne!(*repo, RepoSelector::Query("".into())),
            _ => panic!("wrong action"),
        }
    }

    #[test]
    fn explicit_host_routing_preserved_for_needs_context() {
        let repo_id = RepoIdentity { authority: "github.com".into(), path: "org/repo".into() };
        // Simulate `host feta cr 42 open` — HostNoun::resolve() sets command.host = Some("feta")
        let cmd = Command {
            host: Some(HostName::new("feta")),
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::OpenChangeRequest { id: "42".into() },
        };
        let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Inferred, host: HostResolution::ProviderHost };
        let local_target = ProvisioningTarget::Host { host: HostName::local() };
        let result = tui_dispatch(resolved, None, false, Some(&repo_id), &local_target, &None, false).expect("should succeed");
        // Explicit host must be preserved, not clobbered by ProviderHost resolution
        assert_eq!(result.host, Some(HostName::new("feta")));
    }

    #[test]
    fn contextual_palette_derives_host_from_source_item() {
        let repo_id = RepoIdentity { authority: "github.com".into(), path: "org/repo".into() };
        let item = WorkItem {
            kind: WorkItemKind::ChangeRequest,
            identity: WorkItemIdentity::ChangeRequest("42".into()),
            host: HostName::new("remote-peer"),
            branch: None,
            description: String::new(),
            checkout: None,
            change_request_key: Some("42".into()),
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_checkout: false,
            debug_group: Vec::new(),
            source: None,
            terminal_keys: Vec::new(),
            attachable_set_id: None,
            agent_keys: Vec::new(),
        };
        let cmd = Command {
            host: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::OpenChangeRequest { id: "42".into() },
        };
        let my_host = Some(HostName::new("local-host"));
        // ProviderHost on a remote-only repo should derive host from the item
        let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Inferred, host: HostResolution::ProviderHost };
        let local_target = ProvisioningTarget::Host { host: HostName::local() };
        let result = tui_dispatch(resolved, Some(&item), false, Some(&repo_id), &local_target, &my_host, true).expect("should succeed");
        assert_eq!(result.host, Some(HostName::new("remote-peer")));
    }
}
