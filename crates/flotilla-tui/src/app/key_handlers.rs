use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{Command, CommandAction, WorkItem};

use super::{ui_state::PendingActionContext, App, BranchInputKind, Intent};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    widgets::InteractiveWidget,
};

impl App {
    // ── Key handling ──

    /// Resolve a key event using the app-level config/normal distinction.
    ///
    /// Called when the base layer widget (Normal mode_id) is on top, so that
    /// config mode gets Overview bindings instead of Normal.
    fn resolve_action(&self, key: KeyEvent) -> Option<Action> {
        let mode_id = if self.ui.is_config { BindingModeId::Overview } else { BindingModeId::Normal };
        let mode: KeyBindingMode = mode_id.into();
        self.keymap.resolve(&mode, crokey::KeyCombination::from(key))
    }

    /// Handle actions that the widget stack returned `Ignored` for.
    ///
    /// These are actions that need `&mut App` context the widget doesn't
    /// have: confirm/enter, action menu, file picker, and dispatch intent.
    pub(super) fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Confirm => {
                if !self.ui.is_config {
                    self.action_enter();
                }
            }
            Action::OpenActionMenu => {
                if !self.ui.is_config {
                    self.open_action_menu();
                }
            }
            Action::OpenFilePicker => {
                if !self.ui.is_config {
                    self.open_file_picker_from_active_repo_parent();
                }
            }
            Action::Dispatch(intent) => {
                if !self.ui.is_config {
                    self.dispatch_if_available(intent);
                }
            }
            // Handled by the widget stack (page widgets or modals) or
            // pre-dispatched as global actions. No-op if they reach here.
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Clear the transient command echo on every key press.
        self.ui.command_echo = None;

        // Snapshot selection so we can detect changes for infinite scroll.
        let prev_selection = self.active_page_selection();

        // Determine the topmost widget's mode. Screen delegates to the
        // top modal (if any) for mode_id / captures_raw_keys.
        let captures_raw = self.screen.captures_raw_keys();
        let mode_id = self.screen.binding_mode().primary();

        let action = if captures_raw {
            match key.code {
                // Resolve Enter/Esc through the widget's own binding mode so
                // user overrides for e.g. IssueSearch.confirm still fire.
                KeyCode::Esc | KeyCode::Enter => self.keymap.resolve(&KeyBindingMode::from(mode_id), crokey::KeyCombination::from(key)),
                _ => None,
            }
        } else {
            match mode_id {
                // When the top widget is the base layer (Normal mode_id),
                // resolve using the actual UI mode. This ensures Config mode
                // gets correct bindings (e.g. q → Dismiss, not Quit).
                BindingModeId::Normal => self.resolve_action(key),
                _ => self.keymap.resolve(&KeyBindingMode::from(mode_id), crokey::KeyCombination::from(key)),
            }
        };

        // Dispatch to Screen, which handles modal routing internally.
        // Take the screen out to avoid borrow conflicts between the widget
        // dispatch (`&mut Screen`) and the `WidgetContext` (borrows other `App` fields).
        let mut screen = std::mem::take(&mut self.screen);
        let (outcome_is_ignored, app_actions) = {
            let mut ctx = self.build_widget_context();
            let outcome =
                if let Some(action) = action { screen.handle_action(action, &mut ctx) } else { screen.handle_raw_key(key, &mut ctx) };
            (matches!(outcome, crate::widgets::Outcome::Ignored), std::mem::take(&mut ctx.app_actions))
        };
        self.screen = screen;

        // Fall through if unhandled — these are actions that need &mut App
        // context the widget stack doesn't have. Only when no modal is active:
        // modals are focus barriers, so their Ignored should not leak through
        // to app-level dispatch.
        if outcome_is_ignored && !self.screen.has_modal() {
            if let Some(action) = action {
                self.dispatch_action(action);
            }
        }
        self.process_app_actions(app_actions);

        // Post-dispatch: check for infinite scroll only if the selection
        // actually changed. This avoids spurious fetches from unrelated
        // key presses that happen to fire when the selection is near the bottom.
        if self.active_page_selection() != prev_selection {
            self.check_infinite_scroll();
        }
    }

    // ── Mouse handling ──

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Snapshot selection so we can detect changes for infinite scroll.
        let prev_selection = self.active_page_selection();

        // Dispatch to Screen, which handles modal routing internally.
        let mut screen = std::mem::take(&mut self.screen);
        let app_actions = {
            let mut ctx = self.build_widget_context();
            screen.handle_mouse(mouse, &mut ctx);
            std::mem::take(&mut ctx.app_actions)
        };
        self.screen = screen;
        self.process_app_actions(app_actions);

        // ── Tab drag handling ──
        // The Tabs widget owns the drag state but can't mutate model.repo_order
        // (read-only in WidgetContext). Perform the actual swap here.
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            let tabs = &mut self.screen.tabs;
            if tabs.drag.dragging_tab.is_some()
                && tabs.drag.active
                && tabs.handle_drag(mouse.column, mouse.row, &mut self.model.repo_order, &mut self.model.active_repo)
            {
                // Update drag index to the new position after the swap
                tabs.update_drag_index(self.model.active_repo);
            }
        }

        // ── Infinite scroll check ──
        // Only if the selection actually changed — avoids spurious fetches
        // from tab bar clicks, status bar clicks, etc.
        if self.active_page_selection() != prev_selection {
            self.check_infinite_scroll();
        }
    }

    /// Get the current selection index from the active RepoPage, if any.
    fn active_page_selection(&self) -> Option<usize> {
        if self.model.repo_order.is_empty() {
            return None;
        }
        let identity = &self.model.repo_order[self.model.active_repo];
        self.screen.repo_pages.get(identity).and_then(|page| page.table.selected_flat_index())
    }

    // ── Private helpers ──

    /// Check if the current selection is near the bottom and fetch more issues.
    ///
    /// The SplitTable widget handles selection changes but can't mutate
    /// `model.repos` (to set `issue_fetch_pending`). This post-dispatch check
    /// runs after every key event to trigger infinite scroll when needed.
    fn check_infinite_scroll(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        let identity = &self.model.repo_order[self.model.active_repo];
        let Some(page) = self.screen.repo_pages.get(identity) else {
            return;
        };
        let Some(next) = page.table.selected_flat_index() else {
            return;
        };
        let total = page.table.total_item_count();
        if next + 5 >= total && self.model.active().issue_has_more && !self.model.active().issue_fetch_pending {
            let repo = self.model.active_repo_root().clone();
            let issue_count = self.model.active().providers.issues.len();
            let desired = issue_count + 50;
            let repo_identity = self.model.active_repo_identity().clone();
            if let Some(rm) = self.model.repos.get_mut(&repo_identity) {
                rm.issue_fetch_pending = true;
            }
            self.proto_commands.push(
                self.command(CommandAction::FetchMoreIssues { repo: flotilla_protocol::RepoSelector::Path(repo), desired_count: desired }),
            );
        }
    }

    pub(super) fn action_enter(&mut self) {
        let identity = &self.model.repo_order[self.model.active_repo];
        let has_multi = self.screen.repo_pages.get(identity).is_some_and(|p| !p.multi_selected.is_empty());
        if has_multi {
            self.action_enter_multi_select();
            return;
        }

        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = self.model.my_host().cloned();
        for &intent in Intent::enter_priority() {
            if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
                self.resolve_and_push(intent, &item);
                return;
            }
        }
    }

    fn action_enter_multi_select(&mut self) {
        let identity = &self.model.repo_order[self.model.active_repo];
        let Some(page) = self.screen.repo_pages.get(identity) else {
            return;
        };
        let multi_selected = page.multi_selected.clone();
        let mut all_issue_keys: Vec<String> = Vec::new();

        // Collect issues from multi-selected items
        for item in page.table.all_items() {
            if multi_selected.contains(&item.identity) {
                all_issue_keys.extend(item.issue_keys.iter().cloned());
            }
        }

        // Also include current selection if not already in multi_selected
        if let Some(item) = self.selected_work_item() {
            if !multi_selected.contains(&item.identity) {
                all_issue_keys.extend(item.issue_keys.iter().cloned());
            }
        }

        all_issue_keys.sort();
        all_issue_keys.dedup();
        if !all_issue_keys.is_empty() {
            self.screen.modal_stack.push(Box::new(crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Generating)));
            self.proto_commands.push(self.targeted_repo_command(CommandAction::GenerateBranchName { issue_keys: all_issue_keys }));
        }
        let identity = identity.clone();
        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
            page.multi_selected.clear();
        }
    }

    fn dispatch_if_available(&mut self, intent: Intent) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };
        let my_host = self.model.my_host().cloned();
        if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
            self.resolve_and_push(intent, &item);
        }
    }

    fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
        // Safety net: block filesystem operations on remote items even if
        // the caller somehow bypassed the menu/availability filter.
        let my_host = self.model.my_host().cloned();
        if !intent.is_allowed_for_host(item, &my_host) {
            tracing::warn!(?intent, host = %item.host, "blocked intent on remote item");
            self.model.status_message = Some("Cannot perform this action on a remote item".to_string());
            return;
        }

        // Try registry path for convertible intents.
        if let Some(noun) = intent.to_noun_command(item) {
            self.ui.command_echo = Some(noun.to_string());

            match noun.resolve() {
                Ok(resolved) => {
                    let is_config = self.ui.is_config;
                    let active_repo = if is_config { None } else { Some(self.model.active_repo_identity()) };
                    let provisioning_target = self.ui.provisioning_target.clone();
                    let remote_only = self.active_repo_is_remote_only();

                    match crate::widgets::command_palette::tui_dispatch(
                        resolved,
                        Some(item),
                        is_config,
                        active_repo,
                        &provisioning_target,
                        &my_host,
                        remote_only,
                    ) {
                        Ok(cmd) => {
                            // Modal handling for convertible intents that need confirmation
                            match intent {
                                Intent::CloseChangeRequest => {
                                    let id = match &cmd {
                                        Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                                        _ => return,
                                    };
                                    let widget = crate::widgets::close_confirm::CloseConfirmWidget::new(
                                        id,
                                        item.description.clone(),
                                        item.identity.clone(),
                                        cmd,
                                    );
                                    self.screen.modal_stack.push(Box::new(widget));
                                    return;
                                }
                                Intent::GenerateBranchName => {
                                    self.screen
                                        .modal_stack
                                        .push(Box::new(crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Generating)));
                                }
                                _ => {}
                            }
                            let pending_ctx = PendingActionContext {
                                identity: item.identity.clone(),
                                description: intent.label(self.model.active_labels()),
                                repo_identity: self.model.active_repo_identity().clone(),
                            };
                            self.proto_commands.push_with_context(cmd, Some(pending_ctx));
                            return;
                        }
                        Err(e) => {
                            self.model.status_message = Some(e);
                            return;
                        }
                    }
                }
                Err(e) => {
                    // Registry parse failed — clear stale echo and fall back to old path
                    self.ui.command_echo = None;
                    tracing::warn!(%e, ?intent, "registry parse failed, falling back to intent.resolve");
                }
            }
        }

        // Non-convertible intents (or registry fallback): use old path
        if let Some(cmd) = intent.resolve(item, self) {
            match intent {
                Intent::RemoveCheckout => {
                    let checkout_path = item.checkout_key().map(|hp| hp.path.clone());
                    let widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(
                        item.identity.clone(),
                        self.item_execution_host(item),
                        checkout_path,
                    );
                    self.screen.modal_stack.push(Box::new(widget));
                }
                Intent::GenerateBranchName => {
                    self.screen
                        .modal_stack
                        .push(Box::new(crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Generating)));
                }
                Intent::CloseChangeRequest => {
                    let id = match &cmd {
                        Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                        _ => return,
                    };
                    let widget =
                        crate::widgets::close_confirm::CloseConfirmWidget::new(id, item.description.clone(), item.identity.clone(), cmd);
                    self.screen.modal_stack.push(Box::new(widget));
                    return;
                }
                _ => {}
            }
            let pending_ctx = PendingActionContext {
                identity: item.identity.clone(),
                description: intent.label(self.model.active_labels()),
                repo_identity: self.model.active_repo_identity().clone(),
            };
            self.proto_commands.push_with_context(cmd, Some(pending_ctx));
        }
    }

    pub(super) fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = self.model.my_host().cloned();
        let entries: Vec<crate::widgets::action_menu::MenuEntry> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter_map(|intent| {
                if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
                    intent.resolve(&item, self).map(|command| crate::widgets::action_menu::MenuEntry { intent, command })
                } else {
                    None
                }
            })
            .collect();

        if entries.is_empty() {
            return;
        }

        self.screen.modal_stack.push(Box::new(crate::widgets::action_menu::ActionMenuWidget::new(entries, item)));
    }
}

#[cfg(test)]
mod tests;
