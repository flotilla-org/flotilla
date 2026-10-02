use std::{any::Any, collections::HashSet, path::PathBuf};

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
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
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    file_picker_model::DirectoryListing,
    keymap::Action,
    ui_helpers,
};

#[derive(bon::Builder)]
pub struct FilePickerWidget {
    input: Input,
    #[builder(skip)]
    dir_entries: Vec<DirEntry>,
    #[builder(skip)]
    listing: DirectoryListing,
    #[builder(skip)]
    tracked_paths: HashSet<PathBuf>,
    #[builder(skip)]
    selected: usize,
    #[builder(skip)]
    picker_area: Rect,
    #[builder(skip)]
    list_area: Rect,
}

impl FilePickerWidget {
    /// Opening the picker does no filesystem work. The first render schedules
    /// discovery on a worker and subsequent frames collect its results.
    pub fn open(input: Input) -> Self {
        Self::builder().input(input).build()
    }

    #[cfg(test)]
    pub(crate) fn new(input: Input, mut dir_entries: Vec<DirEntry>) -> Self {
        use crate::file_picker_model::Directory;

        let base = if input.value().ends_with('/') {
            PathBuf::from(input.value())
        } else {
            PathBuf::from(input.value()).parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf()
        };
        for entry in &mut dir_entries {
            entry.path = base.join(&entry.name);
        }
        let entries = dir_entries
            .iter()
            .map(|entry| Directory { name: entry.name.clone(), path: entry.path.clone(), is_git_repo: entry.is_git_repo })
            .collect();
        let listing = DirectoryListing::seeded(input.value(), entries);
        let mut widget = Self::open(input);
        widget.dir_entries = dir_entries;
        widget.listing = listing;
        widget
    }

    /// Create a file picker with a pre-set selection index.
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected;
        self
    }

    fn refresh_dir_listing(&mut self, model: &TuiModel) {
        let changed = self.listing.update(self.input.value());
        let tracked: HashSet<_> = model.repos.values().map(|repo| repo.path.clone()).collect();
        if changed || tracked != self.tracked_paths {
            self.dir_entries = self
                .listing
                .entries()
                .iter()
                .map(|entry| {
                    DirEntry::builder()
                        .name(entry.name.clone())
                        .path(entry.path.clone())
                        .is_dir(true)
                        .is_git_repo(entry.is_git_repo)
                        .is_added(tracked.contains(&entry.path))
                        .build()
                })
                .collect();
            self.tracked_paths = tracked;
            self.selected = self.selected.min(self.dir_entries.len().saturating_sub(1));
        }
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
            let canonical = entry.path;
            let cmd = Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::TrackRepoPath { path: canonical.clone() },
            };
            ctx.commands.push(cmd);
            // Adding via [+] both registers the repo and opens its tab once
            // the daemon confirms with RepoTracked.
            ctx.app_actions.push(super::AppAction::ExpectRepoOpen(canonical));
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
            Action::FillSelected => {
                if let Some(entry) = self.dir_entries.get(self.selected).cloned() {
                    let base = self.base_path();
                    let new_path = format!("{}{}/", base, entry.name);
                    self.input = Input::from(new_path.as_str());
                    self.selected = 0;
                }
                self.refresh_dir_listing(ctx.model);
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        // Only reached for unresolved keys (typing) because FilePicker uses
        // no_shared_fallback; navigation keys are handled via handle_action.
        self.input.handle_event(&crossterm::event::Event::Key(key));
        self.selected = 0;
        self.refresh_dir_listing(ctx.model);
        Outcome::Consumed
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
        self.refresh_dir_listing(ctx.model);
        let theme = ctx.theme;

        let (popup_area, inner) = ui_helpers::render_popup_frame(frame, area, 60, 60, " Add Repository ", theme.block_style());
        self.picker_area = popup_area;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
            .split(inner);

        self.list_area = chunks[2];

        let input_text = self.input.value();
        let display = format!("> {}", input_text);
        let paragraph = Paragraph::new(display).style(Style::default().fg(theme.input_text));
        frame.render_widget(paragraph, chunks[0]);

        let cursor_x = chunks[0].x + 2 + self.input.visual_cursor() as u16;
        frame.set_cursor_position((cursor_x, chunks[0].y));

        let status = self.listing.error().unwrap_or_else(|| if self.listing.is_loading() { "Loading directories..." } else { "" });
        let status_style =
            if self.listing.error().is_some() { Style::default().fg(theme.status_error) } else { Style::default().fg(theme.muted) };
        frame.render_widget(Paragraph::new(status).style(status_style), chunks[1]);

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
        frame.render_stateful_widget(list, chunks[2], &mut state);
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::FilePicker.into()
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::Label("ADD REPO".into())) }
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
    use flotilla_protocol::{Command, CommandAction};

    use super::*;
    use crate::{
        app::{test_support::TestWidgetHarness, UiState},
        theme::Theme,
    };

    fn dir_entry(name: &str, is_git_repo: bool, is_added: bool) -> DirEntry {
        DirEntry::builder()
            .name(name.to_string())
            .path(PathBuf::from(name))
            .is_dir(true)
            .is_git_repo(is_git_repo)
            .is_added(is_added)
            .build()
    }

    fn picker_with_entries(path: &str, entries: Vec<DirEntry>) -> FilePickerWidget {
        FilePickerWidget::new(Input::from(path), entries)
    }

    fn render_picker(
        widget: &mut FilePickerWidget,
        harness: &mut TestWidgetHarness,
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> String {
        let mut ui = UiState::new(&[]);
        let theme = Theme::classic();
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
            .expect("render picker");
        terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect()
    }

    #[tokio::test]
    async fn loading_hint_and_results_render_without_keypresses() {
        let tmp = tempfile::tempdir().expect("temporary directory");
        std::fs::create_dir(tmp.path().join("loaded-child")).expect("child directory");
        let input = format!("{}/", tmp.path().display());
        let mut widget = FilePickerWidget::open(Input::from(input.as_str()));
        let mut harness = TestWidgetHarness::new();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("test terminal");
        assert!(render_picker(&mut widget, &mut harness, &mut terminal).contains("Loading directories..."));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let rendered = render_picker(&mut widget, &mut harness, &mut terminal);
                if rendered.contains("loaded-child/") {
                    assert!(!rendered.contains("Loading directories..."));
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subsequent frames show worker results without input");
    }

    #[tokio::test]
    async fn directory_errors_remain_visible_in_the_picker() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let input = format!("{}/missing/", tmp.path().display());
        let mut widget = FilePickerWidget::open(Input::from(input.as_str()));
        let mut harness = TestWidgetHarness::new();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("test terminal");
        let mut ui = UiState::new(&[]);
        let theme = Theme::classic();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
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
                    .expect("render picker");
                let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
                if rendered.contains("Unable to read directory") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("directory failure must be visible, rather than an empty list");
    }

    #[tokio::test]
    async fn binding_mode_is_file_picker() {
        let widget = FilePickerWidget::new(Input::default(), vec![]);
        assert_eq!(widget.binding_mode(), KeyBindingMode::from(BindingModeId::FilePicker));
    }

    #[tokio::test]
    async fn does_not_capture_raw_keys() {
        let widget = FilePickerWidget::new(Input::default(), vec![]);
        assert!(!widget.captures_raw_keys());
    }

    #[tokio::test]
    async fn dismiss_returns_finished() {
        let mut widget = picker_with_entries("/tmp/", vec![dir_entry("foo", false, false)]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[tokio::test]
    async fn select_next_advances() {
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        let mut widget = picker_with_entries("/tmp/", entries);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::SelectNext, &mut ctx);
        assert_eq!(widget.selected, 1);
    }

    #[tokio::test]
    async fn select_next_clamps_at_end() {
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        let mut widget = picker_with_entries("/tmp/", entries);
        let mut harness = TestWidgetHarness::new();

        for _ in 0..5 {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx);
        }
        assert_eq!(widget.selected, 1);
    }

    #[tokio::test]
    async fn select_prev_saturates_at_zero() {
        let entries = vec![dir_entry("aaa", false, false)];
        let mut widget = picker_with_entries("/tmp/", entries);
        let mut harness = TestWidgetHarness::new();

        for _ in 0..3 {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectPrev, &mut ctx);
        }
        assert_eq!(widget.selected, 0);
    }

    #[tokio::test]
    async fn select_next_noop_on_empty() {
        let mut widget = picker_with_entries("/tmp/", vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::SelectNext, &mut ctx);
        assert_eq!(widget.selected, 0);
    }

    #[tokio::test]
    async fn fill_selected_completes_directory_name() {
        let entries = vec![dir_entry("alpha", false, false), dir_entry("bar", false, false)];
        let mut widget = FilePickerWidget::new(Input::from("foo/"), entries).with_selected(1);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::FillSelected, &mut ctx);
        assert_eq!(widget.input.value(), "foo/bar/");
        assert_eq!(widget.selected, 0);
    }

    #[tokio::test]
    async fn confirm_on_git_repo_pushes_track_command() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&repo_dir).expect("create repo dir");
        std::fs::create_dir(repo_dir.join(".git")).expect("create .git dir");

        let parent_path = format!("{}/", tmp.path().to_string_lossy());
        let entries = vec![dir_entry("my-repo", true, false)];
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

    #[tokio::test]
    async fn confirm_on_directory_navigates_into_it() {
        let entries = vec![dir_entry("subdir", false, false)];
        let mut widget = picker_with_entries("/base/path/", entries);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "/base/path/subdir/");
        assert_eq!(widget.selected, 0);
    }

    #[tokio::test]
    async fn confirm_with_no_entries_does_nothing() {
        let mut widget = picker_with_entries("/tmp/", vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(harness.commands.take_next().is_none());
    }

    #[tokio::test]
    async fn confirm_on_added_git_repo_navigates_into_it() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let sub = tmp.path().join("existing-repo");
        std::fs::create_dir(&sub).expect("create dir");
        std::fs::create_dir(sub.join(".git")).expect("create .git");

        let base = format!("{}/", tmp.path().display());
        let entries = vec![dir_entry("existing-repo", true, true)];
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

    #[tokio::test]
    async fn unhandled_action_returns_ignored() {
        let mut widget = picker_with_entries("/tmp/", vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    // ── refresh_dir_listing tests (filesystem-backed) ─────────────────

    async fn settle_widget(widget: &mut FilePickerWidget, harness: &TestWidgetHarness) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                widget.refresh_dir_listing(&harness.model);
                if !widget.listing.is_loading() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("directory scan finishes");
        assert!(widget.listing.error().is_none(), "{:?}", widget.listing.error());
    }

    async fn picker_for_tmpdir(tmp: &std::path::Path, harness: &TestWidgetHarness) -> FilePickerWidget {
        let path_str = format!("{}/", tmp.display());
        let mut widget = FilePickerWidget::open(Input::from(path_str.as_str()));
        settle_widget(&mut widget, harness).await;
        widget
    }

    #[tokio::test]
    async fn refresh_lists_entries_sorted_alphabetically() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("bravo")).expect("create dir");
        std::fs::create_dir(tmp.path().join("alpha")).expect("create dir");
        std::fs::create_dir(tmp.path().join("charlie")).expect("create dir");

        let harness = TestWidgetHarness::new();
        let widget = picker_for_tmpdir(tmp.path(), &harness).await;

        let names: Vec<&str> = widget.dir_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[tokio::test]
    async fn refresh_hides_dotfiles() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join(".hidden")).expect("create dir");
        std::fs::create_dir(tmp.path().join("visible")).expect("create dir");

        let harness = TestWidgetHarness::new();
        let widget = picker_for_tmpdir(tmp.path(), &harness).await;

        let names: Vec<&str> = widget.dir_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["visible"]);
    }

    #[tokio::test]
    async fn refresh_detects_git_repos() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&repo_dir).expect("create dir");
        std::fs::create_dir(repo_dir.join(".git")).expect("create .git");
        std::fs::create_dir(tmp.path().join("not-a-repo")).expect("create dir");

        let harness = TestWidgetHarness::new();
        let widget = picker_for_tmpdir(tmp.path(), &harness).await;

        let git_entry = widget.dir_entries.iter().find(|e| e.name == "my-repo").expect("should find my-repo");
        assert!(git_entry.is_git_repo);
        let non_git = widget.dir_entries.iter().find(|e| e.name == "not-a-repo").expect("should find not-a-repo");
        assert!(!non_git.is_git_repo);
    }

    #[tokio::test]
    async fn refresh_marks_added_repos() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("tracked");
        std::fs::create_dir(&repo_dir).expect("create dir");
        std::fs::create_dir(tmp.path().join("untracked")).expect("create dir");
        let canonical = std::fs::canonicalize(&repo_dir).expect("canonicalize");

        let mut harness = TestWidgetHarness::new();
        // Point the stub repo's path at our tracked directory
        let first_repo = harness.model.repo_order[0].clone();
        harness.model.repos.get_mut(&first_repo).expect("repo").path = canonical;

        let widget = picker_for_tmpdir(tmp.path(), &harness).await;

        let tracked = widget.dir_entries.iter().find(|e| e.name == "tracked").expect("tracked");
        assert!(tracked.is_added, "tracked repo should be marked as added");
        let untracked = widget.dir_entries.iter().find(|e| e.name == "untracked").expect("untracked");
        assert!(!untracked.is_added, "untracked dir should not be marked as added");
    }

    #[tokio::test]
    async fn refresh_filters_by_prefix() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("alpha")).expect("create dir");
        std::fs::create_dir(tmp.path().join("beta")).expect("create dir");

        let harness = TestWidgetHarness::new();
        // Type "al" as a prefix filter (no trailing slash = filter mode)
        let path_str = format!("{}/al", tmp.path().display());
        let mut widget = FilePickerWidget::open(Input::from(path_str.as_str()));
        settle_widget(&mut widget, &harness).await;

        let names: Vec<&str> = widget.dir_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
    }
}
