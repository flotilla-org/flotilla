use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use tui_input::backend::crossterm::EventHandler as InputEventHandler;
use tui_input::Input;

use flotilla_protocol::Command;

use super::{App, DirEntry, UiMode};

impl App {
    pub(super) fn handle_file_picker_key(&mut self, key: KeyEvent) {
        // Keys that change mode
        if key.code == KeyCode::Esc {
            self.ui.mode = UiMode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            self.activate_dir_entry();
            return;
        }

        let needs_refresh = {
            let UiMode::FilePicker {
                ref mut input,
                ref mut dir_entries,
                ref mut selected,
            } = self.ui.mode
            else {
                return;
            };
            match key.code {
                KeyCode::Down | KeyCode::Char('j')
                    if key.modifiers.is_empty() || key.code == KeyCode::Down =>
                {
                    if !dir_entries.is_empty() {
                        *selected = (*selected + 1).min(dir_entries.len() - 1);
                    }
                    false
                }
                KeyCode::Up | KeyCode::Char('k')
                    if key.modifiers.is_empty() || key.code == KeyCode::Up =>
                {
                    *selected = selected.saturating_sub(1);
                    false
                }
                KeyCode::Tab => {
                    if let Some(entry) = dir_entries.get(*selected).cloned() {
                        let current = input.value().to_string();
                        let base = if current.ends_with('/') {
                            current.clone()
                        } else {
                            current
                                .rsplit_once('/')
                                .map(|(prefix, _)| format!("{prefix}/"))
                                .unwrap_or_default()
                        };
                        let new_path = format!("{}{}/", base, entry.name);
                        *input = Input::from(new_path.as_str());
                        *selected = 0;
                    }
                    true
                }
                _ => {
                    input.handle_event(&crossterm::event::Event::Key(key));
                    *selected = 0;
                    true
                }
            }
        };
        if needs_refresh {
            self.refresh_dir_listing();
        }
    }

    fn activate_dir_entry(&mut self) {
        let (entry, base) = {
            let UiMode::FilePicker {
                ref input,
                ref dir_entries,
                selected,
            } = self.ui.mode
            else {
                return;
            };
            let Some(entry) = dir_entries.get(selected).cloned() else {
                return;
            };
            let current = input.value().to_string();
            let base = if current.ends_with('/') {
                current
            } else {
                current
                    .rsplit_once('/')
                    .map(|(prefix, _)| format!("{prefix}/"))
                    .unwrap_or_default()
            };
            (entry, base)
        };

        if entry.is_git_repo && !entry.is_added {
            let path = PathBuf::from(format!("{}{}", base, entry.name));
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            self.proto_commands
                .push(Command::AddRepo { path: canonical });
            self.ui.mode = UiMode::Normal;
        } else if entry.is_dir {
            let new_path = format!("{}{}/", base, entry.name);
            if let UiMode::FilePicker {
                ref mut input,
                ref mut selected,
                ..
            } = self.ui.mode
            {
                *input = Input::from(new_path.as_str());
                *selected = 0;
            }
            self.refresh_dir_listing();
        }
    }

    pub(super) fn handle_file_picker_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let x = mouse.column;
        let y = mouse.row;
        let a = self.ui.layout.file_picker_area;
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            self.ui.mode = UiMode::Normal;
            return;
        }
        let la = self.ui.layout.file_picker_list_area;
        if x >= la.x && x < la.x + la.width && y >= la.y && y < la.y + la.height {
            let row = (y - la.y) as usize;
            let len = if let UiMode::FilePicker {
                ref dir_entries, ..
            } = self.ui.mode
            {
                dir_entries.len()
            } else {
                return;
            };
            if row < len {
                if let UiMode::FilePicker {
                    ref mut selected, ..
                } = self.ui.mode
                {
                    *selected = row;
                }
                self.activate_dir_entry();
            }
        }
    }

    pub fn refresh_dir_listing(&mut self) {
        let Self { model, ui, .. } = self;
        let UiMode::FilePicker {
            ref input,
            ref mut dir_entries,
            ..
        } = ui.mode
        else {
            return;
        };

        let path_str = input.value().to_string();
        let dir = if path_str.ends_with('/') {
            PathBuf::from(&path_str)
        } else {
            PathBuf::from(&path_str)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default()
        };

        let filter = if !path_str.ends_with('/') {
            PathBuf::from(&path_str)
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
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
                let is_added = model.repos.contains_key(&canonical);
                entries.push(DirEntry {
                    name,
                    is_dir,
                    is_git_repo,
                    is_added,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        *dir_entries = entries;
    }
}
