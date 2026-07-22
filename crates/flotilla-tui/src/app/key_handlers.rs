use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{Command, CommandAction, ConvoyStartIntent, HostName, IssueSelector, NodeId, RepoIdentity, RepoKey, WorkItem};

use super::{ui_state::PendingActionContext, App, BranchInputKind, Intent, OwnedSelectedRow};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    table_view::{PendingRowContext, TableIntent},
    widgets::{convoy_delete_confirm::ConvoyDeleteConfirmWidget, InteractiveWidget},
};

impl App {
    // ── Key handling ──

    /// Resolve a key event using the active View's binding-mode stack
    /// (`view_kind::binding_mode`: shell + kind, tab keys only when the tab
    /// bar exists).
    ///
    /// Called when the base layer widget (Normal mode_id) is on top.
    fn resolve_action(&self, key: KeyEvent) -> Option<Action> {
        let mode = crate::app::view_kind::binding_mode(self.views.active_address(), self.views.is_scoped());
        self.keymap.resolve(&mode, crokey::KeyCombination::from(key)).filter(|action| {
            crate::interaction::InteractionContext::for_active_view(
                self.views.active_address(),
                self.views.active_table_state().selected(),
                self.model.active_repo_identity_opt().is_some(),
            )
            .is_available(*action)
        })
    }

    /// Handle actions that the widget stack returned `Ignored` for.
    ///
    /// These are actions that need `&mut App` context the widget doesn't
    /// have: confirm/enter, action menu, file picker, and dispatch intent.
    pub(super) fn dispatch_action(&mut self, action: Action) {
        // Scoped panes: Esc walks the in-place navigation history.
        if action == Action::Dismiss && self.views.is_scoped() {
            self.scoped_back();
            return;
        }
        // Repo-scoped fallthrough actions apply only on a live repo view.
        if self.model.active_repo.is_none() {
            return;
        }
        match action {
            Action::Confirm => self.action_enter(),
            Action::OpenActionMenu => self.open_action_menu(),
            Action::OpenFilePicker => self.open_file_picker_from_active_repo_parent(),
            Action::Dispatch(intent) => self.dispatch_if_available(intent),
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
        // The Tabs widget owns the drag state but can't mutate the open-view
        // set (read-only in WidgetContext). Perform the actual swap here.
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            let tabs = &mut self.screen.tabs;
            if tabs.drag.dragging_tab.is_some() && tabs.drag.active && tabs.handle_drag(mouse.column, mouse.row, &mut self.views) {
                self.sync_active_view();
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
        let identity = self.model.active_repo.as_ref()?;
        self.screen.repo_pages.get(identity).and_then(|page| page.table.selected_flat_index())
    }

    // ── Private helpers ──

    /// Check if the current selection is near the bottom of the issue section
    /// and fetch more results if the active paging state has more pages.
    fn check_infinite_scroll(&mut self) {
        let Some(repo_identity) = self.model.active_repo.clone() else { return };
        let Some(page) = self.screen.repo_pages.get(&repo_identity) else { return };
        let Some((issue_idx, _issue_count)) = page.table.selected_issue_position() else { return };

        let Some(view) = self.issue_views.get(&repo_identity) else { return };
        let Some(active) = view.active() else { return };
        if !active.has_more || active.fetch_pending() {
            return;
        }
        let issue_count = active.items.len();
        if issue_count == 0 {
            return;
        }
        if issue_count.saturating_sub(issue_idx + 1) <= 5 {
            let params = active.params.clone();
            let next_page = active.next_page;
            if !self.begin_issue_page_fetch(&repo_identity, &params, next_page) {
                return;
            }
            self.spawn_query_page(repo_identity, params, next_page, 50);
        }
    }

    pub(super) fn action_enter(&mut self) {
        let Some(identity) = self.model.active_repo.as_ref() else { return };
        let has_multi = self.screen.repo_pages.get(identity).is_some_and(|p| !p.multi_selected.is_empty());
        if has_multi {
            self.action_enter_multi_select();
            return;
        }

        let Some(selected) = self.selected_row_cloned() else {
            return;
        };

        match selected {
            OwnedSelectedRow::WorkItem(ref item) => {
                let my_node_id = self.model.my_node_id().cloned();
                for &intent in Intent::enter_priority() {
                    if intent.is_available(item) && intent.is_allowed_for_host(item, &my_node_id) {
                        self.resolve_and_push(intent, item);
                        return;
                    }
                }
            }
            OwnedSelectedRow::IssueRow(row) => {
                // For issue rows, the primary action is OpenIssue.
                let cmd = self.provider_repo_command_for_issue(CommandAction::OpenIssue { id: row.id });
                self.proto_commands.push(cmd);
            }
        }
    }

    fn action_enter_multi_select(&mut self) {
        let Some(identity) = self.model.active_repo.as_ref() else { return };
        let Some(page) = self.screen.repo_pages.get(identity) else {
            return;
        };
        let multi_selected = page.multi_selected.clone();
        let mut all_issue_keys: Vec<String> = Vec::new();

        for selected in &multi_selected {
            if let Some(issue_keys) = page.table.issue_keys_for_identity(selected) {
                all_issue_keys.extend(issue_keys);
            }
        }

        // Also include current selection if not already in multi_selected
        if let Some(selected_identity) = page.table.selected_identity() {
            if !multi_selected.contains(&selected_identity) {
                if let Some(issue_keys) = page.table.issue_keys_for_identity(&selected_identity) {
                    all_issue_keys.extend(issue_keys);
                }
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
        let Some(item) = (match self.selected_row_cloned() {
            Some(OwnedSelectedRow::WorkItem(item)) => Some(*item),
            Some(OwnedSelectedRow::IssueRow(row)) => Some(self.work_item_for_issue_row(&row)),
            None => None,
        }) else {
            return;
        };
        let my_node_id = self.model.my_node_id().cloned();
        if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_node_id) {
            self.resolve_and_push(intent, &item);
        }
    }

    fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
        // Safety net: block filesystem operations on remote items even if
        // the caller somehow bypassed the menu/availability filter.
        let my_node_id = self.model.my_node_id().cloned();
        if !intent.is_allowed_for_host(item, &my_node_id) {
            tracing::warn!(?intent, node_id = %item.node_id, "blocked intent on remote item");
            self.set_status_message(Some("Cannot perform this action on a remote item".to_string()));
            return;
        }

        // Try registry path for convertible intents.
        if let Some(noun) = intent.to_noun_command(item) {
            self.ui.command_echo = Some(noun.to_string());

            match noun.resolve() {
                Ok(resolved) => {
                    let active_repo = self.model.active_repo.clone();
                    let provisioning_target = self.ui.provisioning_target.clone();
                    let remote_only = self.active_repo_is_remote_only();

                    match crate::widgets::command_palette::tui_dispatch(
                        resolved,
                        &self.model,
                        Some(item),
                        active_repo.as_ref(),
                        &provisioning_target,
                        &my_node_id,
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
                            let pending_ctx = PendingActionContext::work_item(
                                item.identity.clone(),
                                self.model.active_repo_identity().clone(),
                                intent.label(self.model.active_labels()),
                            );
                            self.proto_commands.push_with_context(cmd, Some(pending_ctx));
                            return;
                        }
                        Err(e) => {
                            self.set_status_message(Some(e));
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
            let pending_ctx = PendingActionContext::work_item(
                item.identity.clone(),
                self.model.active_repo_identity().clone(),
                intent.label(self.model.active_labels()),
            );
            self.proto_commands.push_with_context(cmd, Some(pending_ctx));
        }
    }

    fn table_intent_node_id(&mut self, host: Option<&HostName>) -> Result<Option<NodeId>, ()> {
        let Some(host) = host else {
            return Ok(None);
        };
        match self.panel_target_node(host) {
            Ok(node_id) => Ok(node_id),
            Err(message) => {
                self.set_status_message(Some(message));
                Err(())
            }
        }
    }

    pub(super) fn execute_table_intent(&mut self, intent: TableIntent) {
        let (mut command, host) = match intent {
            TableIntent::OpenInPm(target) => {
                let locally_homed = target
                    .host
                    .as_ref()
                    .is_some_and(|home| home == &HostName::local() || self.model.my_host().is_some_and(|local| home == local));
                if !locally_homed {
                    let home = target.host.as_ref().map_or_else(|| "unknown host".to_string(), ToString::to_string);
                    self.set_status_message(Some(format!("{} is not reachable from this PM yet (homed on {home})", target.label)));
                    return;
                }
                let Some(connector) = self.pm_connector.clone() else {
                    self.set_status_message(Some("No presentation manager is connected".to_string()));
                    return;
                };
                let working_directory = self
                    .table_action_repo(target.repo_hint.as_ref())
                    .and_then(|identity| self.model.repos.get(&identity).map(|repo| repo.path.clone()))
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let tx = self.pm_update_tx.clone();
                let label = target.label.clone();
                self.report_focus(vec![target.resource_ref()]);
                self.set_status_message(Some(format!("Opening {label} in PM...")));
                tokio::spawn(async move {
                    let result = connector.open(&target, &working_directory).await;
                    let _ = tx.send(super::PmOpenUpdate { label, result });
                });
                return;
            }
            TableIntent::AttachWorkspace { workspace_ref, host, repo_hint } => {
                let Some(repo_identity) = self.table_action_repo(repo_hint.as_ref()) else {
                    self.set_status_message(Some("Cannot attach workspace: the convoy does not identify a tracked repository".to_string()));
                    return;
                };
                (self.repo_command_for_identity(repo_identity, CommandAction::SelectWorkspace { ws_ref: workspace_ref }), host)
            }
            TableIntent::AttachPane { reference, host } => {
                self.proto_commands.push(self.command(CommandAction::AttachTransient { reference, host: Some(host) }));
                return;
            }
            TableIntent::DeleteConvoy { row_id, namespace, name, host } => {
                let Ok(node_id) = self.table_intent_node_id(host.as_ref()) else {
                    return;
                };
                let mut command =
                    self.command(CommandAction::ConvoyDelete { namespace: Some(namespace.clone()), name: name.clone(), force: false });
                command.node_id = node_id;
                let Some(address) = self.views.active_address().cloned() else {
                    self.set_status_message(Some("Cannot delete convoy: active view has no address".into()));
                    return;
                };
                let panel = matches!(&address, flotilla_protocol::ViewAddress::Project { .. })
                    .then(|| self.views.active_project_table_state().active());
                let pending = PendingActionContext::table_row(
                    PendingRowContext { address, panel, query: flotilla_protocol::QueryId::Convoys, row_id },
                    "Delete convoy".into(),
                );
                self.screen.modal_stack.push(Box::new(ConvoyDeleteConfirmWidget::new(command, pending)));
                return;
            }
            TableIntent::OpenChangeRequest { id, repository_key, host } => {
                let Some(repo_identity) = self
                    .model
                    .repos
                    .iter()
                    .find_map(|(identity, repo)| (repo.repository_key.as_ref() == Some(&repository_key)).then(|| identity.clone()))
                else {
                    self.set_status_message(Some(format!("Cannot open PR: repository {repository_key} is not tracked")));
                    return;
                };
                let Ok(node_id) = self.table_intent_node_id(host.as_ref()) else {
                    return;
                };
                let mut command = self.repo_command_for_identity(repo_identity, CommandAction::OpenChangeRequest { id });
                command.node_id = node_id;
                self.proto_commands.push(command);
                return;
            }
            TableIntent::ForceCompleteWork { convoy, vessel, host } => {
                (self.command(CommandAction::ConvoyWorkForceComplete { convoy, work: vessel, message: None }), host)
            }
            TableIntent::StartConvoy { namespace, project, issue } => {
                self.proto_commands.push(self.command(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: Some(namespace),
                        project_ref: project,
                        issues: vec![IssueSelector::Reference(issue)],
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        auto_attach: true,
                    }),
                }));
                return;
            }
            TableIntent::StartConvoys { namespace, project, issues } => {
                let Some(address) = self.views.active_address().cloned() else { return };
                let batch_id = self.begin_project_issue_start_batch(issues.len());
                for issue in issues {
                    let command = self.command(CommandAction::ConvoyStart {
                        intent: Box::new(ConvoyStartIntent {
                            namespace: Some(namespace.clone()),
                            project_ref: project.clone(),
                            issues: vec![IssueSelector::Reference(issue.issue.clone())],
                            name: None,
                            branch: None,
                            workflow_ref: None,
                            inputs: Vec::new(),
                            instruction: None,
                            placement_policy: None,
                            auto_attach: false,
                        }),
                    });
                    let pending_ctx = PendingActionContext::project_issue_start(
                        crate::app::ui_state::ProjectIssueStartContext {
                            address: address.clone(),
                            row_id: issue.row_id,
                            issue: issue.issue,
                            batch_id,
                        },
                        "Start convoy".into(),
                    );
                    self.proto_commands.push_with_context(command, Some(pending_ctx));
                }
                if let Some(index) = self.views.find(&address) {
                    if let Some(view) = self.views.get_mut(index) {
                        view.project_table_state.table_mut(crate::table_view::ProjectPanelKind::Issues).multi_selected.clear();
                    }
                }
                return;
            }
            TableIntent::StartBatchConvoy { namespace, project, issues } => {
                let Some(address) = self.views.active_address().cloned() else { return };
                let issue_count = issues.len();
                self.proto_commands.push(self.command(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: Some(namespace),
                        project_ref: project,
                        issues: issues.into_iter().map(|issue| IssueSelector::Reference(issue.issue)).collect(),
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction: None,
                        placement_policy: None,
                        auto_attach: true,
                    }),
                }));
                if let Some(index) = self.views.find(&address) {
                    if let Some(view) = self.views.get_mut(index) {
                        view.project_table_state.table_mut(crate::table_view::ProjectPanelKind::Issues).multi_selected.clear();
                    }
                }
                self.set_status_message(Some(format!("Starting batch convoy for {issue_count} issues...")));
                return;
            }
        };
        let node_id = match self.panel_target_node(&host) {
            Ok(node_id) => node_id,
            Err(message) => {
                self.set_status_message(Some(message));
                return;
            }
        };
        command.node_id = node_id;
        self.proto_commands.push(command);
    }

    fn table_action_repo(&self, hint: Option<&RepoKey>) -> Option<RepoIdentity> {
        hint.and_then(|hint| self.model.repo_order.iter().find(|identity| repo_identity_matches_hint(identity, hint)).cloned())
            .or_else(|| self.model.active_repo.clone())
            .or_else(|| (self.model.repo_order.len() == 1).then(|| self.model.repo_order[0].clone()))
    }

    fn panel_target_node(&self, host: &HostName) -> Result<Option<NodeId>, String> {
        if host == &HostName::local() {
            return Ok(None);
        }
        self.model.node_id_for_host(host).cloned().map(Some).ok_or_else(|| format!("host '{}' is not connected", host.as_str()))
    }

    pub(super) fn open_action_menu(&mut self) {
        let Some(item) = (match self.selected_row_cloned() {
            Some(OwnedSelectedRow::WorkItem(item)) => Some(*item),
            Some(OwnedSelectedRow::IssueRow(row)) => Some(self.work_item_for_issue_row(&row)),
            None => None,
        }) else {
            return;
        };

        let my_node_id = self.model.my_node_id().cloned();
        let entries: Vec<crate::widgets::action_menu::MenuEntry> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter_map(|intent| {
                if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_node_id) {
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

pub(super) fn repo_identity_matches_hint(identity: &RepoIdentity, hint: &RepoKey) -> bool {
    if hint.0 == identity.path || hint.0 == format!("{}/{}", identity.authority, identity.path.trim_start_matches('/')) {
        return true;
    }
    if matches!(identity.authority.as_str(), "local" | "unknown") {
        return false;
    }
    let url = format!("https://{}/{}", identity.authority, identity.path.trim_start_matches('/'));
    flotilla_resources::canonicalize_repo_url(&url).is_ok_and(|canonical| flotilla_resources::descriptive_repo_slug(&canonical) == hint.0)
}

#[cfg(test)]
mod tests;
