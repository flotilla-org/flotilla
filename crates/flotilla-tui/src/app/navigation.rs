use flotilla_protocol::ViewAddress;

use super::App;

impl App {
    pub fn switch_tab(&mut self, idx: usize) {
        if self.views.switch_to(idx) {
            self.dismiss_modals();
            self.sync_active_view();
        }
    }

    pub fn next_tab(&mut self) {
        self.dismiss_modals();
        self.views.step(1);
        self.sync_active_view();
    }

    pub fn prev_tab(&mut self) {
        self.dismiss_modals();
        self.views.step(-1);
        self.sync_active_view();
    }

    /// Focus the View at `address`, opening a tab for it if needed. Scoped
    /// sessions navigate in place (with a back stack) instead of growing a
    /// tab set — the pane stays one View (ADR 0013).
    pub fn open_view(&mut self, address: ViewAddress) {
        self.dismiss_modals();
        if self.views.open_or_focus(address) {
            self.subscriptions_dirty = true;
            self.persist_open_views();
        }
        self.sync_active_view();
    }

    /// Navigate in place inside the active tab. The previous address and its
    /// table state remain on that tab's history stack.
    pub fn drill_view(&mut self, address: ViewAddress) {
        self.dismiss_modals();
        if self.views.drill(address) {
            self.subscriptions_dirty = true;
            self.persist_open_views();
            self.sync_active_view();
        }
    }

    /// Pop the active tab's in-place navigation history.
    pub fn back_view(&mut self) -> bool {
        if self.views.back() {
            self.dismiss_modals();
            self.subscriptions_dirty = true;
            self.persist_open_views();
            self.sync_active_view();
            true
        } else {
            false
        }
    }

    /// Scoped-mode back navigation: return to the View the last in-place
    /// open left behind. Returns true if navigation happened.
    pub fn scoped_back(&mut self) -> bool {
        self.back_view()
    }

    /// Return to the previously active tab (overview dismiss).
    pub fn switch_to_last_view(&mut self) {
        if self.views.switch_to_last() {
            self.dismiss_modals();
            self.sync_active_view();
        }
    }

    /// Move the active tab by `delta`. Returns true if the order changed.
    pub fn move_tab(&mut self, delta: isize) -> bool {
        let moved = self.views.move_tab(self.views.active_index(), delta).is_some();
        if moved {
            self.persist_open_views();
        }
        moved
    }

    /// Close the tab at `idx` (the pinned overview refuses). Returns true if
    /// a tab was closed.
    pub fn close_tab(&mut self, idx: usize) -> bool {
        let closed = self.views.close(idx);
        if closed {
            self.dismiss_modals();
            self.subscriptions_dirty = true;
            self.sync_active_view();
            self.persist_open_views();
        }
        closed
    }

    pub fn close_active_tab(&mut self) -> bool {
        self.close_tab(self.views.active_index())
    }

    #[cfg(test)]
    pub(super) fn select_next(&mut self) {
        let Some(identity) = self.model.active_repo.clone() else { return };
        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
            let total = page.table.total_item_count();
            if total == 0 {
                return;
            }
            page.table.select_next();
        }
    }

    #[cfg(test)]
    pub(super) fn select_prev(&mut self) {
        let Some(identity) = self.model.active_repo.clone() else { return };
        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
            page.table.select_prev();
        }
    }
}

#[cfg(test)]
mod tests;
