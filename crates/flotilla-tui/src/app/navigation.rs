use flotilla_core::data::GroupEntry;
use flotilla_protocol::Command;

use super::{App, UiMode};

impl App {
    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.model.repo_order.len() {
            self.ui.mode = UiMode::Normal;
            self.model.active_repo = idx;
            let key = &self.model.repo_order[idx];
            self.ui
                .repo_ui
                .get_mut(key)
                .expect("active repo must have UI state")
                .has_unseen_changes = false;
        }
    }

    pub fn next_tab(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        if self.ui.mode.is_config() {
            self.ui.mode = UiMode::Normal;
            self.model.active_repo = 0;
        } else if self.model.active_repo < self.model.repo_order.len() - 1 {
            self.switch_tab(self.model.active_repo + 1);
        } else {
            self.ui.mode = UiMode::Config;
        }
    }

    pub fn prev_tab(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        if self.ui.mode.is_config() {
            self.ui.mode = UiMode::Normal;
            self.model.active_repo = self.model.repo_order.len() - 1;
        } else if self.model.active_repo > 0 {
            self.switch_tab(self.model.active_repo - 1);
        } else {
            self.ui.mode = UiMode::Config;
        }
    }

    pub fn move_tab(&mut self, delta: isize) -> bool {
        let len = self.model.repo_order.len();
        if len < 2 {
            return false;
        }
        let cur = self.model.active_repo;
        let new_idx = cur as isize + delta;
        if new_idx < 0 || new_idx >= len as isize {
            return false;
        }
        let new_idx = new_idx as usize;
        self.model.repo_order.swap(cur, new_idx);
        self.model.active_repo = new_idx;
        true
    }

    pub(super) fn select_next(&mut self) {
        let indices = &self.active_ui().table_view.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = self.active_ui().selected_selectable_idx;
        let next = match current_si {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = self.active_ui().table_view.selectable_indices[next];
        self.active_ui_mut().selected_selectable_idx = Some(next);
        self.active_ui_mut().table_state.select(Some(table_idx));

        // Infinite scroll: fetch more issues when near the bottom
        let total = self.active_ui().table_view.selectable_indices.len();
        if next + 5 >= total
            && self.model.active().issue_has_more
            && !self.model.active().issue_fetch_pending
        {
            let repo = self.model.active_repo_root().clone();
            let issue_count = self.model.active().providers.issues.len();
            let desired = issue_count + 50;
            if let Some(rm) = self.model.repos.get_mut(&repo) {
                rm.issue_fetch_pending = true;
            }
            self.proto_commands.push(Command::FetchMoreIssues {
                repo,
                desired_count: desired,
            });
        }
    }

    pub(super) fn select_prev(&mut self) {
        let indices = &self.active_ui().table_view.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = self.active_ui().selected_selectable_idx;
        let prev = match current_si {
            Some(si) if si > 0 => si - 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = self.active_ui().table_view.selectable_indices[prev];
        self.active_ui_mut().selected_selectable_idx = Some(prev);
        self.active_ui_mut().table_state.select(Some(table_idx));
    }

    pub(super) fn row_at_mouse(&self, x: u16, y: u16) -> Option<usize> {
        let ta = self.ui.layout.table_area;
        if x >= ta.x && x < ta.x + ta.width && y >= ta.y && y < ta.y + ta.height {
            let row_in_table = (y - ta.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let offset = self.active_ui().table_state.offset();
            let actual_row = data_row + offset;
            self.active_ui()
                .table_view
                .selectable_indices
                .iter()
                .position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    pub(super) fn toggle_multi_select(&mut self) {
        if let Some(si) = self.active_ui().selected_selectable_idx {
            if let Some(&table_idx) = self.active_ui().table_view.selectable_indices.get(si) {
                if let Some(GroupEntry::Item(item)) =
                    self.active_ui().table_view.table_entries.get(table_idx)
                {
                    let identity = item.identity.clone();
                    let rui = self.active_ui_mut();
                    if !rui.multi_selected.remove(&identity) {
                        rui.multi_selected.insert(identity);
                    }
                }
            }
        }
    }
}
