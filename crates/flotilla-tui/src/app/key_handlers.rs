use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{CommandAction, ConvoyStartIntent, HostName, IssueSelector, NodeId, RepoIdentity, RepoKey};

use super::{ui_state::PendingActionContext, App};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    table_view::{PendingRowContext, TableIntent},
    widgets::{convoy_delete_confirm::ConvoyDeleteConfirmWidget, dispatch_confirm::DispatchConfirmWidget, InteractiveWidget},
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
    /// These are actions that need `&mut App` context the widget doesn't have.
    pub(super) fn dispatch_action(&mut self, action: Action) {
        // Scoped panes: Esc walks the in-place navigation history.
        if action == Action::Dismiss && self.views.is_scoped() {
            self.scoped_back();
            return;
        }
        let _ = action;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Clear the transient command echo on every key press.
        self.ui.command_echo = None;

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
    }

    // ── Mouse handling ──

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
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
                self.proto_commands.push(self.command(CommandAction::AttachTransient {
                    reference,
                    host: Some(host),
                    mode: flotilla_protocol::commands::AttachMode::Default,
                }));
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
                    PendingRowContext { address, panel, query: flotilla_protocol::QueryId::Convoys { scope: None }, row_id },
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
                    self.set_status_message(Some("Cannot open PR: repository is not tracked".to_string()));
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
            intent @ (TableIntent::StartConvoy { .. } | TableIntent::StartConvoys { .. } | TableIntent::StartBatchConvoy { .. }) => {
                self.screen.modal_stack.push(Box::new(DispatchConfirmWidget::new(intent)));
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

    pub(super) fn execute_confirmed_convoy_dispatch(&mut self, intent: TableIntent, instruction: Option<String>) {
        match intent {
            TableIntent::StartConvoy { namespace, project, issue } => {
                self.proto_commands.push(self.command(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: Some(namespace),
                        project_ref: project,
                        change_request: None,
                        issues: vec![IssueSelector::Reference(issue.issue)],
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction,
                        placement_policy: None,
                        agent_overrides: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Default,
                    }),
                }));
            }
            TableIntent::StartConvoys { namespace, project, issues } => {
                let Some(address) = self.views.active_address().cloned() else { return };
                let batch_id = self.begin_project_issue_start_batch(issues.len());
                for issue in issues {
                    let command = self.command(CommandAction::ConvoyStart {
                        intent: Box::new(ConvoyStartIntent {
                            namespace: Some(namespace.clone()),
                            project_ref: project.clone(),
                            change_request: None,
                            issues: vec![IssueSelector::Reference(issue.issue.clone())],
                            name: None,
                            branch: None,
                            workflow_ref: None,
                            inputs: Vec::new(),
                            instruction: instruction.clone(),
                            placement_policy: None,
                            agent_overrides: Vec::new(),
                            auto_attach: flotilla_protocol::ConvoyAutoAttach::Never,
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
            }
            TableIntent::StartBatchConvoy { namespace, project, issues } => {
                let Some(address) = self.views.active_address().cloned() else { return };
                let issue_count = issues.len();
                self.proto_commands.push(self.command(CommandAction::ConvoyStart {
                    intent: Box::new(ConvoyStartIntent {
                        namespace: Some(namespace),
                        project_ref: project,
                        change_request: None,
                        issues: issues.into_iter().map(|issue| IssueSelector::Reference(issue.issue)).collect(),
                        name: None,
                        branch: None,
                        workflow_ref: None,
                        inputs: Vec::new(),
                        instruction,
                        placement_policy: None,
                        agent_overrides: Vec::new(),
                        auto_attach: flotilla_protocol::ConvoyAutoAttach::Default,
                    }),
                }));
                if let Some(index) = self.views.find(&address) {
                    if let Some(view) = self.views.get_mut(index) {
                        view.project_table_state.table_mut(crate::table_view::ProjectPanelKind::Issues).multi_selected.clear();
                    }
                }
                self.set_status_message(Some(format!("Starting batch convoy for {issue_count} issues...")));
            }
            _ => unreachable!("dispatch confirmation only returns convoy-start table intents"),
        }
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
