use std::{any::Any, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{Command, CommandAction};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::{ui_state::DirEntry, TuiModel},
    binding_table::BindingModeId,
    keymap::Action,
    ui_helpers,
};

pub struct FilePickerWidget {
    input: Input,
    dir_entries: Vec<DirEntry>,
    selected: usize,
    picker_area: Rect,
    list_area: Rect,
}

impl FilePickerWidget {
    pub fn new(input: Input, dir_entries: Vec<DirEntry>) -> Self {
        Self { input, dir_entries, selected: 0, picker_area: Rect::default(), list_area: Rect::default() }
    }

    /// Create a file picker with a pre-set selection index.
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected;
        self
    }

    fn refresh_dir_listing(&mut self, model: &TuiModel) {
        let path_str = self.input.value().to_string();
        let dir = if path_str.ends_with('/') {
            PathBuf::from(&path_str)
        } else {
            PathBuf::from(&path_str).parent().map(|p| p.to_path_buf()).unwrap_or_default()
        };

        let filter = if !path_str.ends_with('/') {
            PathBuf::from(&path_str).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
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
        self.dir_entries = entries;
    }

    fn base_path(&self) -> String {
        let current = self.input.value().to_string();
        if current.ends_with('/') {
            current
        } else {
            current.rsplit_once('/').map(|(prefix, _)| format!("{prefix}/")).unwrap_or_default()
        }
    }

    fn activate_dir_entry(&mut self, ctx: &mut WidgetContext) -> Outcome {
        let Some(entry) = self.dir_entries.get(self.selected).cloned() else {
            return Outcome::Consumed;
        };
        let base = self.base_path();

        if entry.is_git_repo && !entry.is_added {
            let path = PathBuf::from(format!("{}{}", base, entry.name));
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            let cmd = Command { host: None, context_repo: None, action: CommandAction::TrackRepoPath { path: canonical } };
            ctx.commands.push(cmd);
            return Outcome::Finished;
        } else if entry.is_dir {
            let new_path = format!("{}{}/", base, entry.name);
            self.input = Input::from(new_path.as_str());
            self.selected = 0;
            self.refresh_dir_listing(ctx.model);
            return Outcome::Consumed;
        }

        Outcome::Consumed
    }
}

impl InteractiveWidget for FilePickerWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                if !self.dir_entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.dir_entries.len() - 1);
                }
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.selected = self.selected.saturating_sub(1);
                Outcome::Consumed
            }
            Action::Confirm => self.activate_dir_entry(ctx),
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        match key.code {
            KeyCode::Tab => {
                if let Some(entry) = self.dir_entries.get(self.selected).cloned() {
                    let base = self.base_path();
                    let new_path = format!("{}{}/", base, entry.name);
                    self.input = Input::from(new_path.as_str());
                    self.selected = 0;
                }
                self.refresh_dir_listing(ctx.model);
                Outcome::Consumed
            }
            _ => {
                self.input.handle_event(&crossterm::event::Event::Key(key));
                self.selected = 0;
                self.refresh_dir_listing(ctx.model);
                Outcome::Consumed
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Outcome::Ignored;
        }

        let x = mouse.column;
        let y = mouse.row;
        let a = self.picker_area;

        // Click outside dismisses
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            return Outcome::Finished;
        }

        let la = self.list_area;
        if x >= la.x && x < la.x + la.width && y >= la.y && y < la.y + la.height {
            let row = (y - la.y) as usize;
            if row < self.dir_entries.len() {
                self.selected = row;
                return self.activate_dir_entry(ctx);
            }
        }

        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let theme = ctx.theme;

        let (popup_area, inner) = ui_helpers::render_popup_frame(frame, area, 60, 60, " Add Repository ", theme.block_style());
        self.picker_area = popup_area;

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(inner);

        self.list_area = chunks[1];

        let input_text = self.input.value();
        let display = format!("> {}", input_text);
        let paragraph = Paragraph::new(display).style(Style::default().fg(theme.input_text));
        frame.render_widget(paragraph, chunks[0]);

        let cursor_x = chunks[0].x + 2 + self.input.visual_cursor() as u16;
        frame.set_cursor_position((cursor_x, chunks[0].y));

        let items: Vec<ListItem> = self
            .dir_entries
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

        let list = List::new(items).highlight_style(Style::default().bg(theme.row_highlight).bold()).highlight_symbol("\u{25b8} ");

        let mut state = ListState::default();
        if !self.dir_entries.is_empty() {
            state.select(Some(self.selected));
        }
        frame.render_stateful_widget(list, chunks[1], &mut state);
    }

    fn mode_id(&self) -> BindingModeId {
        BindingModeId::FilePicker
    }

    fn captures_raw_keys(&self) -> bool {
        false
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
    use flotilla_protocol::{Command, CommandAction};

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dir_entry(name: &str, is_git_repo: bool, is_added: bool) -> DirEntry {
        DirEntry { name: name.to_string(), is_dir: true, is_git_repo, is_added }
    }

    fn picker_with_entries(path: &str, entries: Vec<DirEntry>) -> FilePickerWidget {
        FilePickerWidget::new(Input::from(path), entries)
    }

    #[test]
    fn mode_id_is_file_picker() {
        let widget = FilePickerWidget::new(Input::default(), vec![]);
        assert_eq!(widget.mode_id(), BindingModeId::FilePicker);
    }

    #[test]
    fn does_not_capture_raw_keys() {
        let widget = FilePickerWidget::new(Input::default(), vec![]);
        assert!(!widget.captures_raw_keys());
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = picker_with_entries("/tmp/", vec![dir_entry("foo", false, false)]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn select_next_advances() {
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        let mut widget = picker_with_entries("/tmp/", entries);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::SelectNext, &mut ctx);
        assert_eq!(widget.selected, 1);
    }

    #[test]
    fn select_next_clamps_at_end() {
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        let mut widget = picker_with_entries("/tmp/", entries);
        let mut harness = TestWidgetHarness::new();

        for _ in 0..5 {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx);
        }
        assert_eq!(widget.selected, 1);
    }

    #[test]
    fn select_prev_saturates_at_zero() {
        let entries = vec![dir_entry("aaa", false, false)];
        let mut widget = picker_with_entries("/tmp/", entries);
        let mut harness = TestWidgetHarness::new();

        for _ in 0..3 {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectPrev, &mut ctx);
        }
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn select_next_noop_on_empty() {
        let mut widget = picker_with_entries("/tmp/", vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::SelectNext, &mut ctx);
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn tab_completes_directory_name() {
        let entries = vec![dir_entry("alpha", false, false), dir_entry("bar", false, false)];
        let mut widget = FilePickerWidget {
            input: Input::from("foo/"),
            dir_entries: entries,
            selected: 1, // "bar" is selected
            picker_area: Rect::default(),
            list_area: Rect::default(),
        };
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_raw_key(key(KeyCode::Tab), &mut ctx);
        assert_eq!(widget.input.value(), "foo/bar/");
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn confirm_on_git_repo_pushes_track_command() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&repo_dir).expect("create repo dir");
        std::fs::create_dir(repo_dir.join(".git")).expect("create .git dir");

        let parent_path = format!("{}/", tmp.path().to_string_lossy());
        let entries = vec![DirEntry { name: "my-repo".to_string(), is_dir: true, is_git_repo: true, is_added: false }];
        let mut widget = picker_with_entries(&parent_path, entries);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected a command");
        match cmd {
            Command { action: CommandAction::TrackRepoPath { path }, .. } => {
                let canonical = std::fs::canonicalize(&repo_dir).expect("canonicalize");
                assert_eq!(path, canonical);
            }
            other => panic!("expected TrackRepoPath, got {:?}", other),
        }
    }

    #[test]
    fn confirm_on_directory_navigates_into_it() {
        let entries = vec![dir_entry("subdir", false, false)];
        let mut widget = picker_with_entries("/base/path/", entries);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "/base/path/subdir/");
        assert_eq!(widget.selected, 0);
    }

    #[test]
    fn confirm_with_no_entries_does_nothing() {
        let mut widget = picker_with_entries("/tmp/", vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_on_added_git_repo_navigates_into_it() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let sub = tmp.path().join("existing-repo");
        std::fs::create_dir(&sub).expect("create dir");
        std::fs::create_dir(sub.join(".git")).expect("create .git");

        let base = format!("{}/", tmp.path().display());
        let entries = vec![DirEntry { name: "existing-repo".to_string(), is_dir: true, is_git_repo: true, is_added: true }];
        let mut widget = picker_with_entries(&base, entries);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        // is_added=true, so it falls through to the is_dir branch and navigates
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), format!("{base}existing-repo/"));
        assert_eq!(widget.selected, 0);
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = picker_with_entries("/tmp/", vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    // ── refresh_dir_listing tests (filesystem-backed) ─────────────────

    fn picker_for_tmpdir(tmp: &std::path::Path, harness: &TestWidgetHarness) -> FilePickerWidget {
        let path_str = format!("{}/", tmp.display());
        let mut widget = FilePickerWidget::new(Input::from(path_str.as_str()), Vec::new());
        widget.refresh_dir_listing(&harness.model);
        widget
    }

    #[test]
    fn refresh_lists_entries_sorted_alphabetically() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("bravo")).expect("create dir");
        std::fs::create_dir(tmp.path().join("alpha")).expect("create dir");
        std::fs::create_dir(tmp.path().join("charlie")).expect("create dir");

        let harness = TestWidgetHarness::new();
        let widget = picker_for_tmpdir(tmp.path(), &harness);

        let names: Vec<&str> = widget.dir_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn refresh_hides_dotfiles() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join(".hidden")).expect("create dir");
        std::fs::create_dir(tmp.path().join("visible")).expect("create dir");

        let harness = TestWidgetHarness::new();
        let widget = picker_for_tmpdir(tmp.path(), &harness);

        let names: Vec<&str> = widget.dir_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["visible"]);
    }

    #[test]
    fn refresh_detects_git_repos() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&repo_dir).expect("create dir");
        std::fs::create_dir(repo_dir.join(".git")).expect("create .git");
        std::fs::create_dir(tmp.path().join("not-a-repo")).expect("create dir");

        let harness = TestWidgetHarness::new();
        let widget = picker_for_tmpdir(tmp.path(), &harness);

        let git_entry = widget.dir_entries.iter().find(|e| e.name == "my-repo").expect("should find my-repo");
        assert!(git_entry.is_git_repo);
        let non_git = widget.dir_entries.iter().find(|e| e.name == "not-a-repo").expect("should find not-a-repo");
        assert!(!non_git.is_git_repo);
    }

    #[test]
    fn refresh_marks_added_repos() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("tracked");
        std::fs::create_dir(&repo_dir).expect("create dir");
        std::fs::create_dir(tmp.path().join("untracked")).expect("create dir");
        let canonical = std::fs::canonicalize(&repo_dir).expect("canonicalize");

        let mut harness = TestWidgetHarness::new();
        // Point the stub repo's path at our tracked directory
        let first_repo = harness.model.repo_order[0].clone();
        harness.model.repos.get_mut(&first_repo).expect("repo").path = canonical;

        let widget = picker_for_tmpdir(tmp.path(), &harness);

        let tracked = widget.dir_entries.iter().find(|e| e.name == "tracked").expect("tracked");
        assert!(tracked.is_added, "tracked repo should be marked as added");
        let untracked = widget.dir_entries.iter().find(|e| e.name == "untracked").expect("untracked");
        assert!(!untracked.is_added, "untracked dir should not be marked as added");
    }

    #[test]
    fn refresh_filters_by_prefix() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("alpha")).expect("create dir");
        std::fs::create_dir(tmp.path().join("beta")).expect("create dir");

        let harness = TestWidgetHarness::new();
        // Type "al" as a prefix filter (no trailing slash = filter mode)
        let path_str = format!("{}/al", tmp.path().display());
        let mut widget = FilePickerWidget::new(Input::from(path_str.as_str()), Vec::new());
        widget.refresh_dir_listing(&harness.model);

        let names: Vec<&str> = widget.dir_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
    }
}
