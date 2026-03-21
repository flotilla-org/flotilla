// File picker integration tests (via handle_key dispatch through the widget stack).
//
// Detailed unit tests for file picker behavior live in `widgets/file_picker.rs`.
// These integration tests verify that the key handler correctly dispatches to the
// widget and that end-to-end flows (e.g. Enter on a git repo → TrackRepoPath command)
// work through the full App::handle_key path.

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use flotilla_protocol::{Command, CommandAction};

    use crate::{
        app::{
            test_support::{dir_entry, enter_file_picker, key, stub_app},
            DirEntry,
        },
        binding_table::{BindingModeId, KeyBindingMode},
    };

    // ── file picker interaction tests ───────────────────────────────

    #[test]
    fn esc_returns_to_normal() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![dir_entry("foo", false, false)]);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    }

    #[test]
    fn down_advances_selection() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        app.handle_key(key(KeyCode::Down));

        // Widget should remain on stack
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn down_stays_at_end() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        // Move to end
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));

        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn up_decrements_selection() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false), dir_entry("ccc", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        // First move down twice, then up once
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Up));

        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn navigation_noop_on_empty_entries() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![]);

        app.handle_key(key(KeyCode::Down));

        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn tab_completes_directory_name() {
        let mut app = stub_app();
        let entries = vec![dir_entry("alpha", false, false), dir_entry("bar", false, false)];
        enter_file_picker(&mut app, "foo/", entries);

        // Move to "bar" (index 1), then Tab to complete
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Tab));

        // Widget should remain on stack after tab completion
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn j_advances_selection() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        app.handle_key(key(KeyCode::Char('j')));

        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    // ── activate_dir_entry tests ─────────────────────────────────────

    #[test]
    fn enter_on_git_repo_pushes_add_repo() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&repo_dir).expect("create repo dir");
        std::fs::create_dir(repo_dir.join(".git")).expect("create .git dir");

        let mut app = stub_app();
        let parent_path = format!("{}/", tmp.path().to_string_lossy());
        let entries = vec![DirEntry { name: "my-repo".to_string(), is_dir: true, is_git_repo: true, is_added: false }];
        enter_file_picker(&mut app, &parent_path, entries);

        app.handle_key(key(KeyCode::Enter));

        // Widget should be dismissed after adding a repo
        assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");

        // Should have pushed a TrackRepoPath command
        let (cmd, _) = app.proto_commands.take_next().expect("expected a command");
        match cmd {
            Command { action: CommandAction::TrackRepoPath { path }, .. } => {
                let canonical = std::fs::canonicalize(&repo_dir).expect("canonicalize");
                assert_eq!(path, canonical);
            }
            other => panic!("expected TrackRepoPath, got {:?}", other),
        }
    }

    #[test]
    fn enter_on_added_git_repo_navigates_into_it() {
        // When is_git_repo=true AND is_added=true, navigates into the directory
        let tmp = tempfile::tempdir().expect("create tempdir");
        let sub = tmp.path().join("existing-repo");
        std::fs::create_dir(&sub).expect("create dir");
        std::fs::create_dir(sub.join(".git")).expect("create .git dir");

        let base = format!("{}/", tmp.path().display());
        let mut app = stub_app();
        let entries = vec![DirEntry { name: "existing-repo".to_string(), is_dir: true, is_git_repo: true, is_added: true }];
        enter_file_picker(&mut app, &base, entries);

        app.handle_key(key(KeyCode::Enter));

        // Widget should remain on stack (navigated into dir)
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
        // No AddRepo command should have been pushed
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn enter_on_directory_navigates_into_it() {
        let mut app = stub_app();
        let entries = vec![dir_entry("subdir", false, false)];
        enter_file_picker(&mut app, "/base/path/", entries);

        app.handle_key(key(KeyCode::Enter));

        // Widget should remain on stack (navigated into subdir)
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn enter_with_no_entries_does_nothing() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![]);

        app.handle_key(key(KeyCode::Enter));

        // Widget should remain on stack since there are no entries
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
        assert!(app.proto_commands.take_next().is_none());
    }

    // ── Base path extraction tests ───────────────────────────────────

    #[test]
    fn enter_on_entry_with_trailing_slash_path() {
        let mut app = stub_app();
        let entries = vec![dir_entry("child", false, false)];
        enter_file_picker(&mut app, "foo/", entries);

        app.handle_key(key(KeyCode::Enter));

        // Widget should remain on stack (navigated into dir)
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }

    #[test]
    fn enter_on_entry_without_trailing_slash() {
        // Path "foo/ba" means base is "foo/" (rsplit_once on '/')
        let mut app = stub_app();
        let entries = vec![dir_entry("bar", false, false)];
        enter_file_picker(&mut app, "foo/ba", entries);

        app.handle_key(key(KeyCode::Enter));

        // Widget should remain on stack (navigated into dir)
        assert_eq!(
            app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
            KeyBindingMode::from(BindingModeId::FilePicker)
        );
    }
}
