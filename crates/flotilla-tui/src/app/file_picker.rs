use std::path::PathBuf;

use super::{App, DirEntry, UiMode};

impl App {
    pub fn refresh_dir_listing(&mut self) {
        let Self { model, ui, .. } = self;
        let UiMode::FilePicker { ref input, ref mut dir_entries, .. } = ui.mode else {
            return;
        };

        let path_str = input.value().to_string();
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
        *dir_entries = entries;
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use flotilla_protocol::{Command, CommandAction, RepoLabels};

    use super::*;
    use crate::app::test_support::{default_repo_model, dir_entry, enter_file_picker, key, stub_app};

    // ── file picker interaction tests ───────────────────────────────

    #[test]
    fn esc_returns_to_normal() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![dir_entry("foo", false, false)]);
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn down_advances_selection() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        app.handle_key(key(KeyCode::Down));

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 1);
        } else {
            panic!("expected FilePicker mode");
        }
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

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 1); // stays at last index
        } else {
            panic!("expected FilePicker mode");
        }
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

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 1);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn up_stays_at_zero() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Up));

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn navigation_noop_on_empty_entries() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![]);

        app.handle_key(key(KeyCode::Down));

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn tab_completes_directory_name() {
        let mut app = stub_app();
        let entries = vec![dir_entry("alpha", false, false), dir_entry("bar", false, false)];
        enter_file_picker(&mut app, "foo/", entries);

        // Move to "bar" (index 1), then Tab to complete
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Tab));

        if let UiMode::FilePicker { ref input, selected, .. } = app.ui.mode {
            assert_eq!(input.value(), "foo/bar/");
            assert_eq!(selected, 0);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn j_advances_selection() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        app.handle_key(key(KeyCode::Char('j')));

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 1);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn k_decrements_selection() {
        let mut app = stub_app();
        let entries = vec![dir_entry("aaa", false, false), dir_entry("bbb", false, false)];
        enter_file_picker(&mut app, "/tmp/", entries);

        // Advance to index 1 first, then move back
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('k')));

        if let UiMode::FilePicker { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected FilePicker mode");
        }
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

        // Mode should be Normal after adding a repo
        assert!(matches!(app.ui.mode, UiMode::Normal));

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
        // When is_git_repo=true AND is_added=true, the code skips the AddRepo
        // branch and falls through to the is_dir branch, navigating into it.
        let tmp = tempfile::tempdir().expect("create tempdir");
        let sub = tmp.path().join("existing-repo");
        std::fs::create_dir(&sub).expect("create dir");
        std::fs::create_dir(sub.join(".git")).expect("create .git dir");

        let base = format!("{}/", tmp.path().display());
        let mut app = stub_app();
        let entries = vec![DirEntry { name: "existing-repo".to_string(), is_dir: true, is_git_repo: true, is_added: true }];
        enter_file_picker(&mut app, &base, entries);

        app.handle_key(key(KeyCode::Enter));

        // It should navigate into the directory (is_dir branch)
        if let UiMode::FilePicker { ref input, selected, .. } = app.ui.mode {
            assert_eq!(input.value(), format!("{base}existing-repo/"));
            assert_eq!(selected, 0);
        } else {
            panic!("expected FilePicker mode");
        }

        // No AddRepo command should have been pushed
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn enter_on_directory_navigates_into_it() {
        let mut app = stub_app();
        let entries = vec![dir_entry("subdir", false, false)];
        enter_file_picker(&mut app, "/base/path/", entries);

        app.handle_key(key(KeyCode::Enter));

        if let UiMode::FilePicker { ref input, selected, .. } = app.ui.mode {
            assert_eq!(input.value(), "/base/path/subdir/");
            assert_eq!(selected, 0);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn enter_with_no_entries_does_nothing() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![]);

        app.handle_key(key(KeyCode::Enter));

        // Mode should stay FilePicker since there are no entries to activate
        assert!(matches!(app.ui.mode, UiMode::FilePicker { .. }));
        assert!(app.proto_commands.take_next().is_none());
    }

    // ── Base path extraction tests ───────────────────────────────────

    #[test]
    fn enter_on_entry_with_trailing_slash_path() {
        let mut app = stub_app();
        let entries = vec![dir_entry("child", false, false)];
        enter_file_picker(&mut app, "foo/", entries);

        app.handle_key(key(KeyCode::Enter));

        if let UiMode::FilePicker { ref input, .. } = app.ui.mode {
            assert_eq!(input.value(), "foo/child/");
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn enter_on_entry_without_trailing_slash() {
        // Path "foo/ba" means base is "foo/" (rsplit_once on '/')
        let mut app = stub_app();
        let entries = vec![dir_entry("bar", false, false)];
        enter_file_picker(&mut app, "foo/ba", entries);

        app.handle_key(key(KeyCode::Enter));

        if let UiMode::FilePicker { ref input, .. } = app.ui.mode {
            assert_eq!(input.value(), "foo/bar/");
        } else {
            panic!("expected FilePicker mode");
        }
    }

    // ── refresh_dir_listing tests ────────────────────────────────────

    #[test]
    fn refresh_lists_directories_from_temp_dir() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("alpha")).expect("create alpha dir");
        std::fs::create_dir(tmp.path().join("beta")).expect("create beta dir");
        // Create a regular file (should not appear in results)
        std::fs::write(tmp.path().join("file.txt"), "hello").expect("create file");

        let mut app = stub_app();
        let dir_path = format!("{}/", tmp.path().to_string_lossy());
        enter_file_picker(&mut app, &dir_path, vec![]);

        app.refresh_dir_listing();

        if let UiMode::FilePicker { ref dir_entries, .. } = app.ui.mode {
            let names: Vec<&str> = dir_entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"alpha"), "should contain alpha");
            assert!(names.contains(&"beta"), "should contain beta");
            assert!(!names.contains(&"file.txt"), "should not contain files");
            assert_eq!(dir_entries.len(), 2);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn refresh_filters_hidden_dirs() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join(".hidden")).expect("create .hidden dir");
        std::fs::create_dir(tmp.path().join("visible")).expect("create visible dir");

        let mut app = stub_app();
        let dir_path = format!("{}/", tmp.path().to_string_lossy());
        enter_file_picker(&mut app, &dir_path, vec![]);

        app.refresh_dir_listing();

        if let UiMode::FilePicker { ref dir_entries, .. } = app.ui.mode {
            let names: Vec<&str> = dir_entries.iter().map(|e| e.name.as_str()).collect();
            assert!(!names.contains(&".hidden"), "hidden dirs should be filtered");
            assert!(names.contains(&"visible"));
            assert_eq!(dir_entries.len(), 1);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn refresh_filters_by_prefix() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("Foo")).expect("create Foo dir");
        std::fs::create_dir(tmp.path().join("foobar")).expect("create foobar dir");
        std::fs::create_dir(tmp.path().join("baz")).expect("create baz dir");

        let mut app = stub_app();
        // Path without trailing slash: ".../<tmpdir>/foo" means filter is "foo"
        let filter_path = format!("{}/foo", tmp.path().to_string_lossy());
        enter_file_picker(&mut app, &filter_path, vec![]);

        app.refresh_dir_listing();

        if let UiMode::FilePicker { ref dir_entries, .. } = app.ui.mode {
            let names: Vec<&str> = dir_entries.iter().map(|e| e.name.as_str()).collect();
            // Case-insensitive prefix match: both "Foo" and "foobar" match "foo"
            assert!(names.contains(&"Foo"), "Foo should match (case-insensitive)");
            assert!(names.contains(&"foobar"), "foobar should match");
            assert!(!names.contains(&"baz"), "baz should not match prefix 'foo'");
            assert_eq!(dir_entries.len(), 2);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn refresh_detects_git_repos() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let git_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&git_dir).expect("create my-repo dir");
        std::fs::create_dir(git_dir.join(".git")).expect("create .git dir");

        let plain_dir = tmp.path().join("plain");
        std::fs::create_dir(&plain_dir).expect("create plain dir");

        let mut app = stub_app();
        let dir_path = format!("{}/", tmp.path().to_string_lossy());
        enter_file_picker(&mut app, &dir_path, vec![]);

        app.refresh_dir_listing();

        if let UiMode::FilePicker { ref dir_entries, .. } = app.ui.mode {
            let repo_entry = dir_entries.iter().find(|e| e.name == "my-repo").expect("find my-repo");
            assert!(repo_entry.is_git_repo, "should detect .git subdir");

            let plain_entry = dir_entries.iter().find(|e| e.name == "plain").expect("find plain");
            assert!(!plain_entry.is_git_repo, "plain dir is not a git repo");
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn refresh_marks_added_repos() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("tracked-repo");
        std::fs::create_dir(&repo_dir).expect("create tracked-repo dir");

        let mut app = stub_app();

        // Add the canonical path of repo_dir to model.repos so it's "already added"
        let canonical = std::fs::canonicalize(&repo_dir).expect("canonicalize");
        let repo_identity = flotilla_protocol::RepoIdentity { authority: "local".into(), path: canonical.to_string_lossy().into_owned() };
        let mut model = default_repo_model(RepoLabels::default());
        model.identity = repo_identity.clone();
        model.path = canonical.clone();
        app.model.repos.insert(repo_identity.clone(), model);
        app.model.repo_order[0] = repo_identity;

        let dir_path = format!("{}/", tmp.path().to_string_lossy());
        enter_file_picker(&mut app, &dir_path, vec![]);

        app.refresh_dir_listing();

        if let UiMode::FilePicker { ref dir_entries, .. } = app.ui.mode {
            let entry = dir_entries.iter().find(|e| e.name == "tracked-repo").expect("find tracked-repo");
            assert!(entry.is_added, "repo in model.repos should be marked added");
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn refresh_sorts_alphabetically() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        std::fs::create_dir(tmp.path().join("zulu")).expect("create zulu dir");
        std::fs::create_dir(tmp.path().join("alpha")).expect("create alpha dir");
        std::fs::create_dir(tmp.path().join("mike")).expect("create mike dir");

        let mut app = stub_app();
        let dir_path = format!("{}/", tmp.path().to_string_lossy());
        enter_file_picker(&mut app, &dir_path, vec![]);

        app.refresh_dir_listing();

        if let UiMode::FilePicker { ref dir_entries, .. } = app.ui.mode {
            let names: Vec<&str> = dir_entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["alpha", "mike", "zulu"]);
        } else {
            panic!("expected FilePicker mode");
        }
    }

    #[test]
    fn refresh_noop_when_not_in_file_picker_mode() {
        let mut app = stub_app();
        // Mode is Normal by default
        assert!(matches!(app.ui.mode, UiMode::Normal));

        app.refresh_dir_listing();

        // Should still be Normal — no-op
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }
}
