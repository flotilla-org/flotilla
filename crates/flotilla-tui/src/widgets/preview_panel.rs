use std::any::Any;

use flotilla_protocol::WorkItem;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::{TuiModel, UiState},
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    theme::Theme,
};

/// Standalone preview panel component. Renders the selected work item's
/// details and optionally a debug correlation overlay.
///
/// This is primarily a code-organisation move — the preview panel does not
/// have interactive state yet (future enhancement).
pub struct PreviewPanel;

impl PreviewPanel {
    pub fn new() -> Self {
        Self
    }

    /// Render the preview panel using an explicitly provided selected item.
    /// Called by RepoPage with the item from its own table, bypassing RepoUiState.
    pub fn render_with_item(&self, model: &TuiModel, ui: &UiState, item: Option<&WorkItem>, theme: &Theme, frame: &mut Frame, area: Rect) {
        if ui.show_debug {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            self.render_content_for(model, item, theme, frame, chunks[0]);
            self.render_debug_for(item, theme, frame, chunks[1]);
        } else {
            self.render_content_for(model, item, theme, frame, area);
        }
    }

    fn render_content_for(&self, model: &TuiModel, item: Option<&WorkItem>, theme: &Theme, frame: &mut Frame, area: Rect) {
        let text = if let Some(item) = item {
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
                    let noun = if ses.item_noun.is_empty() {
                        model.active_labels().cloud_agents.noun_capitalized()
                    } else {
                        ses.item_noun.clone()
                    };
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

            if let Some(ref set_id) = item.attachable_set_id {
                lines.push(format!("Set: {}", set_id));
            }

            if !item.terminal_keys.is_empty() {
                for key in &item.terminal_keys {
                    let key_str = key.to_string();
                    if let Some(terminal) = providers.managed_terminals.get(&key_str) {
                        let status = format!("{:?}", terminal.status);
                        let cmd = if terminal.command.is_empty() { String::new() } else { format!(" ({})", terminal.command) };
                        lines.push(format!("Terminal: {} [{}]{}", key.role, status, cmd));
                    } else {
                        lines.push(format!("Terminal: {} [?]", key.role));
                    }
                }
            }

            lines.join("\n")
        } else {
            String::new()
        };

        let preview = Paragraph::new(text).block(Block::bordered().style(theme.block_style()).title(" Preview ")).wrap(Wrap { trim: true });
        frame.render_widget(preview, area);
    }

    fn render_debug_for(&self, item: Option<&WorkItem>, theme: &Theme, frame: &mut Frame, area: Rect) {
        let text = if let Some(item) = item {
            if !item.debug_group.is_empty() {
                item.debug_group.join("\n")
            } else {
                "Not correlated (standalone)".into()
            }
        } else {
            String::new()
        };

        let panel = Paragraph::new(text)
            .block(Block::bordered().style(theme.block_style()).title(" Debug (D to toggle) "))
            .wrap(Wrap { trim: true });
        frame.render_widget(panel, area);
    }
}

impl Default for PreviewPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl InteractiveWidget for PreviewPanel {
    fn handle_action(&mut self, _action: Action, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    fn render(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut RenderContext) {
        // PreviewPanel is rendered by RepoPage via render_with_item().
        // This trait method exists to satisfy InteractiveWidget but is never called.
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::Normal.into()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
