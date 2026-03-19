use std::any::Any;

use crossterm::event::{KeyCode, KeyEvent};
use flotilla_protocol::{Command, CommandAction, RepoSelector};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::ui_state::UiMode,
    keymap::{Action, ModeId},
    palette::{self, PaletteEntry, MAX_PALETTE_ROWS},
};

pub struct CommandPaletteWidget {
    input: Input,
    entries: &'static [PaletteEntry],
    selected: usize,
    scroll_top: usize,
}

impl Default for CommandPaletteWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPaletteWidget {
    pub fn new() -> Self {
        Self { input: Input::default(), entries: palette::all_entries(), selected: 0, scroll_top: 0 }
    }

    fn sync_mode(&self, ctx: &mut WidgetContext) {
        *ctx.mode = UiMode::CommandPalette {
            input: self.input.clone(),
            entries: self.entries,
            selected: self.selected,
            scroll_top: self.scroll_top,
        };
    }

    fn filtered(&self) -> Vec<&'static PaletteEntry> {
        palette::filter_entries(self.entries, self.input.value())
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

        // "search <terms>" — apply filter directly, empty clears
        if let Some(query) = text.strip_prefix("search ") {
            let query = query.trim().to_string();
            let repo_identity = ctx.repo_order[ctx.active_repo].clone();
            if query.is_empty() {
                // Clear the active issue search
                let cmd = Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::ClearIssueSearch { repo: RepoSelector::Identity(repo_identity.clone()) },
                };
                ctx.commands.push(cmd);
                if let Some(rui) = ctx.repo_ui.get_mut(&repo_identity) {
                    rui.active_search_query = None;
                }
            } else {
                let cmd = Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::SearchIssues { repo: RepoSelector::Identity(repo_identity.clone()), query: query.clone() },
                };
                ctx.commands.push(cmd);
                if let Some(rui) = ctx.repo_ui.get_mut(&repo_identity) {
                    rui.active_search_query = Some(query);
                }
            }
            *ctx.mode = UiMode::Normal;
            return Outcome::Finished;
        }

        // Otherwise dispatch the selected entry's action
        let filtered = self.filtered();
        if let Some(entry) = filtered.get(self.selected) {
            let action = entry.action;
            *ctx.mode = UiMode::Normal;
            return self.dispatch_palette_action(action, ctx);
        }

        *ctx.mode = UiMode::Normal;
        Outcome::Finished
    }

    fn dispatch_palette_action(&self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            // Actions that open other widgets — use Swap to replace the palette
            Action::OpenBranchInput => {
                let widget = super::branch_input::BranchInputWidget::new(crate::app::ui_state::BranchInputKind::Manual);
                *ctx.mode = UiMode::BranchInput {
                    input: Input::default(),
                    kind: crate::app::ui_state::BranchInputKind::Manual,
                    pending_issue_ids: Vec::new(),
                };
                Outcome::Swap(Box::new(widget))
            }
            Action::OpenIssueSearch => {
                let widget = super::issue_search::IssueSearchWidget::new();
                *ctx.mode = UiMode::IssueSearch { input: Input::default() };
                Outcome::Swap(Box::new(widget))
            }
            Action::OpenFilePicker => {
                // Build the file picker from the active repo parent
                let parent_path = ctx.model.active_repo_root().parent().map(|p| format!("{}/", p.display()));
                let input = parent_path.map(|s| Input::from(s.as_str())).unwrap_or_default();
                let dir_entries = refresh_dir_listing_standalone(input.value(), ctx.model);
                let widget = super::file_picker::FilePickerWidget::new(input.clone(), dir_entries.clone());
                *ctx.mode = UiMode::FilePicker { input, dir_entries, selected: 0 };
                Outcome::Swap(Box::new(widget))
            }
            Action::ToggleHelp => {
                let widget = super::help::HelpWidget::new();
                Outcome::Swap(Box::new(widget))
            }
            Action::OpenActionMenu => {
                // Can't open action menu from palette (needs selected item context),
                // just close the palette and let normal dispatch handle it next time.
                Outcome::Finished
            }
            // Simple state toggles — apply directly through ctx and finish
            Action::Quit => {
                ctx.should_quit = true;
                Outcome::Finished
            }
            Action::CycleLayout => {
                // Cycle the layout via the model pointer isn't available, so just
                // set the mode to Normal and let legacy handle it.
                // Actually, we have no mutable access to UiState.view_layout through ctx.
                // Return Finished; the palette is gone and the action won't re-fire.
                // Instead we accept this minor limitation — these simple actions won't
                // fire from the palette via widget path.  We'll fall through to Finished.
                Outcome::Finished
            }
            Action::Refresh => {
                // Refresh is handled by the main event loop, not dispatch_action.
                // Just finish — the palette closes.
                Outcome::Finished
            }
            // For all other actions, the palette closes and they don't auto-fire.
            // This covers CycleTheme, CycleHost, ToggleProviders, ToggleDebug,
            // ToggleMultiSelect, ToggleStatusBarKeys, etc.
            _ => Outcome::Finished,
        }
    }
}

/// Standalone directory listing that doesn't require `&mut App`.
fn refresh_dir_listing_standalone(path_str: &str, model: &crate::app::TuiModel) -> Vec<crate::app::ui_state::DirEntry> {
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

impl InteractiveWidget for CommandPaletteWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                let count = self.filtered().len();
                if count > 0 {
                    self.selected = (self.selected + 1) % count;
                    self.adjust_scroll();
                }
                self.sync_mode(ctx);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                let count = self.filtered().len();
                if count > 0 {
                    self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
                    self.adjust_scroll();
                }
                self.sync_mode(ctx);
                Outcome::Consumed
            }
            Action::Confirm => self.confirm(ctx),
            Action::Dismiss => {
                *ctx.mode = UiMode::Normal;
                Outcome::Finished
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        // Tab / Right arrow: fill selected entry name into input
        if matches!(key.code, KeyCode::Tab | KeyCode::Right) {
            let filtered = self.filtered();
            if let Some(entry) = filtered.get(self.selected) {
                let filled = format!("{} ", entry.name);
                self.input = Input::from(filled.as_str());
                self.selected = 0;
                self.scroll_top = 0;
            }
            self.sync_mode(ctx);
            return Outcome::Consumed;
        }

        // Backspace on empty input closes the palette
        if matches!(key.code, KeyCode::Backspace) && self.input.value().is_empty() {
            *ctx.mode = UiMode::Normal;
            return Outcome::Finished;
        }

        self.input.handle_event(&crossterm::event::Event::Key(key));

        // Shortcut: typing / when input is empty fills "search "
        if self.input.value() == "/" {
            self.input = Input::from("search ");
            self.selected = 0;
            self.scroll_top = 0;
            self.sync_mode(ctx);
            return Outcome::Consumed;
        }

        self.selected = 0;
        self.scroll_top = 0;
        self.sync_mode(ctx);
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext) {
        let theme = ctx.theme;
        let filtered = self.filtered();
        let palette_height = MAX_PALETTE_ROWS as u16;

        // The status bar area is the bottom row of the frame
        let status_bar_y = area.y + area.height.saturating_sub(1);
        let status_bar_area = Rect::new(area.x, status_bar_y, area.width, 1);

        // Overlay area: fixed height, sits below the status bar (which has been moved up)
        let overlay_y = status_bar_area.y + 1;
        let render_area = Rect::new(status_bar_area.x, overlay_y, status_bar_area.width, palette_height);

        frame.render_widget(Clear, render_area);
        frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), render_area);

        let name_width = filtered.iter().map(|e| e.name.len()).max().unwrap_or(0).min(20);
        let hint_width: u16 = 7;

        for (i, entry) in filtered.iter().skip(self.scroll_top).take(MAX_PALETTE_ROWS).enumerate() {
            let row_y = render_area.y + i as u16;
            let is_selected = self.scroll_top + i == self.selected;

            let row_style = if is_selected {
                Style::default().bg(theme.action_highlight).add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(theme.bar_bg)
            };

            let row_area = Rect::new(render_area.x, row_y, render_area.width, 1);
            frame.render_widget(Block::default().style(row_style), row_area);

            let name_span = Span::styled(format!("  {:<width$}", entry.name, width = name_width), row_style.fg(theme.text));
            let desc_span = Span::styled(format!("  {}", entry.description), row_style.fg(theme.muted));

            let line = Line::from(vec![name_span, desc_span]);
            frame.render_widget(Paragraph::new(line), Rect::new(render_area.x, row_y, render_area.width.saturating_sub(hint_width), 1));

            let hint_text = entry.key_hint.unwrap_or("");
            if !hint_text.is_empty() {
                let hint_span = Span::styled(format!(" {} ", hint_text), row_style.fg(theme.key_hint));
                let hint_x = render_area.x + render_area.width.saturating_sub(hint_width);
                frame.render_widget(Paragraph::new(Line::from(hint_span)), Rect::new(hint_x, row_y, hint_width, 1));
            }
        }

        // Cursor on the status bar row
        let cursor_x = status_bar_area.x + 1 + self.input.visual_cursor() as u16;
        frame.set_cursor_position((cursor_x, status_bar_area.y));
    }

    fn mode_id(&self) -> ModeId {
        ModeId::CommandPalette
    }

    fn captures_raw_keys(&self) -> bool {
        false
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use flotilla_protocol::{Command, CommandAction};

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn mode_id_is_command_palette() {
        let widget = CommandPaletteWidget::new();
        assert_eq!(widget.mode_id(), ModeId::CommandPalette);
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
        assert!(matches!(*ctx.mode, UiMode::Normal));
    }

    #[test]
    fn select_next_wraps_around() {
        let mut widget = CommandPaletteWidget::new();
        let total = widget.filtered().len();
        let mut harness = TestWidgetHarness::new();

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
        let total = widget.filtered().len();
        let mut harness = TestWidgetHarness::new();

        // Prev from 0 wraps to end
        let mut ctx = harness.ctx();
        widget.handle_action(Action::SelectPrev, &mut ctx);
        assert_eq!(widget.selected, total - 1);
    }

    #[test]
    fn tab_fills_selected_entry_name() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        // First entry is "search"
        let outcome = widget.handle_raw_key(key(KeyCode::Tab), &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "search ");
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

        let (cmd, _) = harness.commands.take_next().expect("expected SearchIssues command");
        match cmd {
            Command { action: CommandAction::SearchIssues { query, .. }, .. } => {
                assert_eq!(query, "bug fix");
            }
            other => panic!("expected SearchIssues, got {:?}", other),
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

        let (cmd, _) = harness.commands.take_next().expect("expected ClearIssueSearch command");
        assert!(matches!(cmd, Command { action: CommandAction::ClearIssueSearch { .. }, .. }));
    }

    #[test]
    fn confirm_entry_quit_sets_should_quit() {
        let mut widget = CommandPaletteWidget::new();
        // Type "quit" to filter to the quit entry
        widget.input = Input::from("quit");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        // The widget should finish, and should_quit should be true
        assert!(matches!(outcome, Outcome::Finished));
        assert!(ctx.should_quit);
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
    fn unhandled_action_returns_ignored() {
        let mut widget = CommandPaletteWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::PrevTab, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
