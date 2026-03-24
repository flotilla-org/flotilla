use super::App;

impl App {
    pub fn switch_tab(&mut self, idx: usize) {
        self.screen.tabs.switch_to(idx, &mut self.model, &mut self.ui);
        self.sync_layout_from_active_page();
    }

    pub fn next_tab(&mut self) {
        self.screen.tabs.next_tab(&mut self.model, &mut self.ui);
        self.sync_layout_from_active_page();
    }

    pub fn prev_tab(&mut self) {
        self.screen.tabs.prev_tab(&mut self.model, &mut self.ui);
        self.sync_layout_from_active_page();
    }

    /// Update `ui.view_layout` from the newly-active RepoPage so the
    /// status bar shows the correct layout indicator after a tab switch.
    fn sync_layout_from_active_page(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        let identity = &self.model.repo_order[self.model.active_repo];
        if let Some(page) = self.screen.repo_pages.get(identity) {
            self.ui.view_layout = page.layout;
        }
    }

    pub fn move_tab(&mut self, delta: isize) -> bool {
        self.screen.tabs.move_tab(delta, &mut self.model)
    }

    #[cfg(test)]
    pub(super) fn select_next(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        let identity = self.model.repo_order[self.model.active_repo].clone();
        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
            let total = page.table.grouped_items.selectable_indices.len();
            if total == 0 {
                return;
            }
            page.table.select_next_self();
            // Infinite scroll: fetch more issues when near the bottom
            if let Some(si) = page.table.selected_selectable_idx {
                if si + 5 >= total && self.model.repos[&identity].issue_has_more && !self.model.repos[&identity].issue_fetch_pending {
                    let repo_path = self.model.repos[&identity].path.clone();
                    let issue_count = self.model.repos[&identity].providers.issues.len();
                    let desired = issue_count + 50;
                    if let Some(rm) = self.model.repos.get_mut(&identity) {
                        rm.issue_fetch_pending = true;
                    }
                    self.proto_commands.push(self.command(flotilla_protocol::CommandAction::FetchMoreIssues {
                        repo: flotilla_protocol::RepoSelector::Path(repo_path),
                        desired_count: desired,
                    }));
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn select_prev(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        let identity = self.model.repo_order[self.model.active_repo].clone();
        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
            page.table.select_prev_self();
        }
    }

    #[cfg(test)]
    pub(super) fn row_at_mouse(&self, x: u16, y: u16) -> Option<usize> {
        let ta = self.ui.layout.table_area;
        if x >= ta.x && x < ta.x + ta.width && y >= ta.y && y < ta.y + ta.height {
            let row_in_table = (y - ta.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let identity = &self.model.repo_order[self.model.active_repo];
            if let Some(page) = self.screen.repo_pages.get(identity) {
                let offset = page.table.table_state.offset();
                let actual_row = data_row + offset;
                page.table.grouped_items.selectable_indices.iter().position(|&idx| idx == actual_row)
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
