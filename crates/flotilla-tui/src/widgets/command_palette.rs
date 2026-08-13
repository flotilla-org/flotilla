use std::any::Any;

use crossterm::event::{KeyCode, KeyEvent};
use flotilla_commands::{resolved::HostQueryKind, HostResolution, RepoContext, Resolved};
use flotilla_protocol::{Command, CommandAction, NodeId, ProvisioningTarget, RepoIdentity, RepoSelector};
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

pub struct CommandPaletteWidget {
    input: Input,
    entries: &'static [PaletteEntry],
    selected: usize,
    scroll_top: usize,
    target_node_id: Option<NodeId>,
}
impl Default for CommandPaletteWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPaletteWidget {
    pub fn new() -> Self {
        Self { input: Input::default(), entries: palette::all_entries(), selected: 0, scroll_top: 0, target_node_id: None }
    }

    /// Create a palette widget with pre-filled input text and selection.
    pub fn with_state(input: Input, selected: usize, scroll_top: usize) -> Self {
        Self { input, entries: palette::all_entries(), selected, scroll_top, target_node_id: None }
    }

    pub fn with_prefill_on_node(text: impl AsRef<str>, target_node_id: Option<NodeId>) -> Self {
        Self { input: Input::from(text.as_ref()), entries: palette::all_entries(), selected: 0, scroll_top: 0, target_node_id }
    }

    /// Current input text (for tests / introspection).
    pub fn input_value(&self) -> &str {
        self.input.value()
    }

    fn filtered(&self, interactions: crate::interaction::InteractionContext<'_>) -> Vec<&'static PaletteEntry> {
        palette::filter_entries(self.entries, self.input.value())
            .into_iter()
            .filter(|entry| interactions.is_available(entry.action))
            .collect()
    }

    /// Compute position-aware completions using model context.
    fn completions(
        &self,
        model: &TuiModel,
        namespaces: &crate::app::NamespaceMap,
        has_repo_context: bool,
        interactions: crate::interaction::InteractionContext<'_>,
    ) -> Vec<PaletteCompletion> {
        palette::palette_completions_with_availability(self.input.value(), model, namespaces, has_repo_context, |action| {
            interactions.is_available(action)
        })
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
                let interactions = crate::interaction::InteractionContext::for_active_view(
                    ctx.views.active_address(),
                    ctx.views.active_table_state().selected(),
                    ctx.model.active_repo_identity_opt().is_some(),
                );
                let filtered = self.filtered(interactions);
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
        let active_repo = ctx.model.active_repo_identity_opt().cloned();
        match tui_dispatch(resolved, ctx.model, active_repo.as_ref(), ctx.provisioning_target) {
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
        let interactions = crate::interaction::InteractionContext::for_active_view(
            ctx.views.active_address(),
            ctx.views.active_table_state().selected(),
            ctx.model.active_repo_identity_opt().is_some(),
        );
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
                if ctx.model.active_repo_identity_opt().is_none() {
                    ctx.app_actions.push(AppAction::ShowStatus("this command requires repository context".into()));
                    return Outcome::Finished;
                }
                ctx.app_actions.push(AppAction::Refresh);
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

/// Dispatch a resolved command with ambient context from the TUI environment.
pub(crate) fn tui_dispatch(
    resolved: Resolved,
    model: &TuiModel,
    active_repo: Option<&RepoIdentity>,
    provisioning_target: &ProvisioningTarget,
) -> Result<Command, String> {
    match resolved {
        Resolved::HostQuery { subject, kind } => {
            let host = model.resolve_host(&subject)?;
            let action = match kind {
                HostQueryKind::Status => CommandAction::QueryHostStatus { target_environment_id: host.environment_id.clone() },
                HostQueryKind::Providers => CommandAction::QueryHostProviders { target_environment_id: host.environment_id.clone() },
            };
            Ok(Command {
                node_id: Some(host.summary.node.node_id.clone()),
                provisioning_target: Some(ProvisioningTarget::Host { host: subject }),
                context_repo: None,
                action,
            })
        }
        Resolved::Ready(cmd) => Ok(cmd),
        Resolved::NeedsContext { mut command, repo, host } => {
            // Repo context from the active tab (None on non-repo views)
            let tab_repo = active_repo.map(|id| RepoSelector::Identity(id.clone()));

            match repo {
                RepoContext::None => {}
                RepoContext::Required => {
                    let repo_sel = tab_repo.ok_or_else(|| "no active repository context".to_string())?;
                    command.context_repo = Some(repo_sel.clone());
                    fill_repo_sentinels(&mut command.action, repo_sel);
                }
                RepoContext::Inferred => {
                    if tab_repo.is_none() {
                        return Err("no active repository context".to_string());
                    }
                    command.context_repo = tab_repo;
                }
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
        let has_repo_context = ctx.model.active_repo_identity_opt().is_some();
        let interactions = crate::interaction::InteractionContext::for_active_view(
            ctx.views.active_address(),
            ctx.views.active_table_state().selected(),
            ctx.model.active_repo_identity_opt().is_some(),
        );
        match action {
            Action::SelectNext => {
                let count = self.completions(ctx.model, ctx.namespaces, has_repo_context, interactions).len();
                if count > 0 {
                    self.selected = (self.selected + 1) % count;
                    self.adjust_scroll();
                }
                Outcome::Consumed
            }
            Action::SelectPrev => {
                let count = self.completions(ctx.model, ctx.namespaces, has_repo_context, interactions).len();
                if count > 0 {
                    self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
                    self.adjust_scroll();
                }
                Outcome::Consumed
            }
            Action::Confirm => self.confirm(ctx),
            Action::Dismiss => Outcome::Finished,
            Action::FillSelected => {
                let completions = self.completions(ctx.model, ctx.namespaces, has_repo_context, interactions);
                if let Some(completion) = completions.get(self.selected) {
                    self.fill_completion(completion);
                }
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        let has_repo_context = ctx.model.active_repo_identity_opt().is_some();
        let interactions = crate::interaction::InteractionContext::for_active_view(
            ctx.views.active_address(),
            ctx.views.active_table_state().selected(),
            ctx.model.active_repo_identity_opt().is_some(),
        );
        // Right arrow: fill selected completion into input (Tab goes through handle_action)
        if matches!(key.code, KeyCode::Right) {
            let completions = self.completions(ctx.model, ctx.namespaces, has_repo_context, interactions);
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

        self.selected = 0;
        self.scroll_top = 0;
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        let theme = ctx.theme;
        let has_repo_context = ctx.model.active_repo_identity_opt().is_some();
        let interactions = crate::interaction::InteractionContext::for_active_view(
            ctx.views.active_address(),
            ctx.views.active_table_state().selected(),
            ctx.model.active_repo_identity_opt().is_some(),
        );
        let completions = self.completions(ctx.model, ctx.namespaces, has_repo_context, interactions);
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
        StatusFragment { status: Some(StatusContent::ActiveInput { prefix: ":".into(), text: self.input.value().to_string() }) }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
