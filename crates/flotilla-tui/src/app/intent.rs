use flotilla_protocol::{CheckoutTarget, Command, CommandAction, HostName, RepoLabels, WorkItem, WorkItemKind};

use super::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    SwitchToWorkspace,
    CreateWorkspace,
    RemoveCheckout,
    CreateCheckout,
    GenerateBranchName,
    OpenChangeRequest,
    OpenIssue,
    LinkIssuesToChangeRequest,
    TeleportSession,
    ArchiveSession,
    CloseChangeRequest,
}

impl Intent {
    pub fn label(&self, labels: &RepoLabels) -> String {
        match self {
            Intent::SwitchToWorkspace => "Switch to workspace".into(),
            Intent::CreateWorkspace => "Create workspace".into(),
            Intent::RemoveCheckout => format!("Remove {}", labels.checkouts.noun),
            Intent::CreateCheckout => format!("Create {}", labels.checkouts.noun),
            Intent::GenerateBranchName => "Generate branch name".into(),
            Intent::OpenChangeRequest => format!("Open {} in browser", labels.change_requests.noun),
            Intent::OpenIssue => "Open issue in browser".into(),
            Intent::LinkIssuesToChangeRequest => {
                format!("Link issues to {}", labels.change_requests.noun)
            }
            Intent::TeleportSession => "Teleport session".into(),
            Intent::ArchiveSession => "Archive session".into(),
            Intent::CloseChangeRequest => format!("Close {}", labels.change_requests.noun),
        }
    }

    pub fn is_available(&self, item: &WorkItem) -> bool {
        match self {
            Intent::SwitchToWorkspace => !item.workspace_refs.is_empty(),
            Intent::CreateWorkspace => item.checkout_key().is_some() && item.workspace_refs.is_empty(),
            Intent::RemoveCheckout => item.checkout_key().is_some() && !item.is_main_checkout,
            Intent::CreateCheckout => item.checkout_key().is_none() && item.branch.is_some(),
            Intent::GenerateBranchName => item.branch.is_none() && !item.issue_keys.is_empty(),
            Intent::OpenChangeRequest => item.change_request_key.is_some(),
            Intent::OpenIssue => !item.issue_keys.is_empty(),
            Intent::LinkIssuesToChangeRequest => {
                item.change_request_key.is_some() && item.checkout_key().is_some() && !item.issue_keys.is_empty()
            }
            Intent::TeleportSession => item.session_key.is_some(),
            Intent::ArchiveSession => item.session_key.is_some(),
            Intent::CloseChangeRequest => item.change_request_key.is_some(),
        }
    }

    /// Whether this intent requires local filesystem access.
    ///
    /// Returns `true` for actions that operate on the local filesystem
    /// (switch workspace, teleport session). These should be hidden for
    /// work items from remote hosts.
    pub fn requires_local_host(&self) -> bool {
        matches!(self, Intent::SwitchToWorkspace | Intent::TeleportSession)
    }

    /// Whether this intent is allowed given the item's host provenance.
    ///
    /// Remote items (where `item.host != my_host`) cannot use intents that
    /// require local filesystem access. If `my_host` is `None`, all items
    /// are treated as local (pre-multi-host compatibility).
    pub fn is_allowed_for_host(&self, item: &WorkItem, my_host: &Option<HostName>) -> bool {
        if !self.requires_local_host() {
            return true;
        }
        match my_host {
            Some(host) => item.host == *host,
            None => true,
        }
    }

    pub fn shortcut_hint(&self, labels: &RepoLabels) -> Option<String> {
        match self {
            Intent::RemoveCheckout => Some(format!("d:remove {}", labels.checkouts.noun)),
            Intent::OpenChangeRequest => {
                if labels.change_requests.abbr.is_empty() {
                    Some("p:show".into())
                } else {
                    Some(format!("p:show {}", labels.change_requests.abbr))
                }
            }
            _ => None,
        }
    }

    /// Resolve an intent into a concrete Command, given the current item and app state.
    /// Returns None if the intent can't be resolved (missing data).
    pub fn resolve(&self, item: &WorkItem, app: &App) -> Option<Command> {
        match self {
            Intent::SwitchToWorkspace => {
                item.workspace_refs.first().map(|ws_ref| app.repo_command(CommandAction::SelectWorkspace { ws_ref: ws_ref.clone() }))
            }
            Intent::CreateWorkspace => item.checkout_key().map(|p| {
                let label =
                    item.branch.clone().unwrap_or_else(|| p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                let command = app.item_host_repo_command(
                    CommandAction::PrepareTerminalForCheckout { checkout_path: p.path.clone(), commands: app.local_template_commands() },
                    item,
                );
                if command.host.is_some() {
                    command
                } else {
                    app.repo_command(CommandAction::CreateWorkspaceForCheckout { checkout_path: p.path.clone(), label })
                }
            }),
            Intent::RemoveCheckout => {
                if item.kind != WorkItemKind::Checkout || item.is_main_checkout {
                    return None;
                }
                let branch = item.branch.as_ref()?.to_string();
                let checkout_path = item.checkout_key().map(|p| p.path.clone());
                let change_request_id = item.change_request_key.clone();
                Some(app.item_host_repo_command(CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id }, item))
            }
            Intent::CreateCheckout => item.branch.as_ref().map(|branch| {
                let target = if item.kind == WorkItemKind::RemoteBranch || item.kind == WorkItemKind::ChangeRequest {
                    CheckoutTarget::Branch(branch.to_string())
                } else {
                    CheckoutTarget::FreshBranch(branch.to_string())
                };
                app.targeted_command(CommandAction::Checkout {
                    repo: flotilla_protocol::RepoSelector::Identity(app.model.active_repo_identity().clone()),
                    target,
                    issue_ids: Vec::new(),
                })
            }),
            Intent::GenerateBranchName => {
                if !item.issue_keys.is_empty() {
                    Some(app.targeted_repo_command(CommandAction::GenerateBranchName { issue_keys: item.issue_keys.clone() }))
                } else {
                    None
                }
            }
            Intent::OpenChangeRequest => item
                .change_request_key
                .as_ref()
                .map(|k| app.provider_repo_command(CommandAction::OpenChangeRequest { id: k.clone() }, item)),
            Intent::OpenIssue => {
                item.issue_keys.first().map(|k| app.provider_repo_command(CommandAction::OpenIssue { id: k.clone() }, item))
            }
            Intent::LinkIssuesToChangeRequest => {
                let change_request_key = item.change_request_key.as_ref()?;
                let co_key = item.checkout_key()?;
                let providers = &app.model.active().providers;
                let cr = providers.change_requests.get(change_request_key.as_str())?;
                let co = providers.checkouts.get(co_key)?;

                // Find issue IDs from checkout that aren't already on the PR
                let pr_issue_ids: std::collections::HashSet<&str> = cr
                    .association_keys
                    .iter()
                    .map(|k| {
                        let flotilla_protocol::AssociationKey::IssueRef(_, id) = k;
                        id.as_str()
                    })
                    .collect();
                let missing: Vec<String> = co
                    .association_keys
                    .iter()
                    .filter_map(|k| {
                        let flotilla_protocol::AssociationKey::IssueRef(_, id) = k;
                        if !pr_issue_ids.contains(id.as_str()) {
                            Some(id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                if missing.is_empty() {
                    return None;
                }
                Some(app.provider_repo_command(
                    CommandAction::LinkIssuesToChangeRequest { change_request_id: change_request_key.clone(), issue_ids: missing },
                    item,
                ))
            }
            Intent::TeleportSession => item.session_key.as_ref().map(|k| {
                app.repo_command(CommandAction::TeleportSession {
                    session_id: k.clone(),
                    branch: item.branch.clone(),
                    checkout_key: item.checkout_key().map(|p| p.path.clone()),
                })
            }),
            Intent::ArchiveSession => {
                item.session_key.as_ref().map(|k| app.provider_repo_command(CommandAction::ArchiveSession { session_id: k.clone() }, item))
            }
            Intent::CloseChangeRequest => {
                let cr_key = item.change_request_key.as_ref()?;
                let providers = &app.model.active().providers;
                let cr = providers.change_requests.get(cr_key.as_str())?;
                if cr.status != flotilla_protocol::ChangeRequestStatus::Open {
                    return None;
                }
                Some(app.provider_repo_command(CommandAction::CloseChangeRequest { id: cr_key.clone() }, item))
            }
        }
    }

    pub fn all_in_menu_order() -> &'static [Intent] {
        &[
            Intent::SwitchToWorkspace,
            Intent::CreateWorkspace,
            Intent::RemoveCheckout,
            Intent::CreateCheckout,
            Intent::GenerateBranchName,
            Intent::OpenChangeRequest,
            Intent::OpenIssue,
            Intent::LinkIssuesToChangeRequest,
            Intent::TeleportSession,
            Intent::ArchiveSession,
            Intent::CloseChangeRequest,
        ]
    }

    pub fn enter_priority() -> &'static [Intent] {
        &[Intent::SwitchToWorkspace, Intent::TeleportSession, Intent::CreateWorkspace, Intent::CreateCheckout, Intent::GenerateBranchName]
    }
}

#[cfg(test)]
mod tests;
