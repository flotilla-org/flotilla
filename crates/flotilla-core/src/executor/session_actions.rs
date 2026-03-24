use std::path::{Path, PathBuf};

use flotilla_protocol::{CommandValue, HostName};
use tracing::{info, warn};

use super::WorkspaceOrchestrator;
use crate::{
    attachable::SharedAttachableStore,
    path_context::ExecutionEnvironmentPath,
    provider_data::ProviderData,
    providers::{
        registry::ProviderRegistry,
        types::{CloudAgentSession, CorrelationKey},
    },
    terminal_manager::TerminalManager,
};

pub(super) struct ReadOnlySessionActionService<'a> {
    registry: &'a ProviderRegistry,
    providers_data: &'a ProviderData,
}

pub(super) struct TeleportSessionActionService<'a> {
    read_only: ReadOnlySessionActionService<'a>,
    repo_root: &'a Path,
    config_base: &'a Path,
    attachable_store: &'a SharedAttachableStore,
    daemon_socket_path: Option<&'a Path>,
    local_host: &'a HostName,
    terminal_manager: Option<&'a TerminalManager>,
}

pub(super) struct TeleportFlow<'a> {
    service: TeleportSessionActionService<'a>,
    checkout_key: Option<&'a PathBuf>,
}

impl<'a> ReadOnlySessionActionService<'a> {
    pub(super) fn new(registry: &'a ProviderRegistry, providers_data: &'a ProviderData) -> Self {
        Self { registry, providers_data }
    }

    pub(super) async fn archive_session_result(&self, session_id: &str) -> CommandValue {
        if let Some(session) = self.providers_data.sessions.get(session_id) {
            info!(%session_id, "archiving session");
            if let Some(key) = session_provider_key(session, session_id) {
                if let Some((_, coding_agent)) = self.registry.cloud_agents.get(key) {
                    match coding_agent.archive_session(session_id).await {
                        Ok(()) => CommandValue::Ok,
                        Err(err) => CommandValue::Error { message: err },
                    }
                } else {
                    CommandValue::Error { message: format!("No coding agent provider: {key}") }
                }
            } else {
                CommandValue::Error { message: format!("Cannot determine provider for session {session_id}") }
            }
        } else {
            CommandValue::Error { message: format!("session not found: {session_id}") }
        }
    }

    pub(super) async fn generate_branch_name_result(&self, issue_keys: &[String]) -> CommandValue {
        let issues: Vec<(String, String)> = issue_keys
            .iter()
            .filter_map(|key| self.providers_data.issues.get(key.as_str()).map(|issue| (key.clone(), issue.title.clone())))
            .collect();

        let issue_id_pairs: Vec<(String, String)> = {
            let provider =
                self.registry.issue_trackers.preferred_name().map(|name| name.to_string()).unwrap_or_else(|| "issues".to_string());
            issues.iter().map(|(id, _title)| (provider.clone(), id.clone())).collect()
        };

        info!(issue_count = issue_keys.len(), "generating branch name");
        let branch_result = if let Some(ai) = self.registry.ai_utilities.preferred() {
            let context: Vec<String> = issues.iter().map(|(id, title)| format!("{} #{}", title, id)).collect();
            let prompt_text = if context.len() == 1 { context[0].clone() } else { context.join("; ") };
            Some(ai.generate_branch_name(&prompt_text).await)
        } else {
            None
        };

        match branch_result {
            Some(Ok(name)) => {
                info!(%name, "AI suggested");
                CommandValue::BranchNameGenerated { name, issue_ids: issue_id_pairs }
            }
            Some(Err(error)) => {
                warn!(%error, "using fallback branch name after AI failure");
                let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
                let name = fallback.join("-");
                CommandValue::BranchNameGenerated { name, issue_ids: issue_id_pairs }
            }
            None => {
                warn!("using fallback branch name without AI provider");
                let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
                let name = fallback.join("-");
                CommandValue::BranchNameGenerated { name, issue_ids: issue_id_pairs }
            }
        }
    }
}

impl<'a> TeleportSessionActionService<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        repo_root: &'a Path,
        registry: &'a ProviderRegistry,
        providers_data: &'a ProviderData,
        config_base: &'a Path,
        attachable_store: &'a SharedAttachableStore,
        daemon_socket_path: Option<&'a Path>,
        local_host: &'a HostName,
        terminal_manager: Option<&'a TerminalManager>,
    ) -> Self {
        Self {
            read_only: ReadOnlySessionActionService::new(registry, providers_data),
            repo_root,
            config_base,
            attachable_store,
            daemon_socket_path,
            local_host,
            terminal_manager,
        }
    }

    pub(super) async fn resolve_teleport_checkout_path(
        &self,
        checkout_key: Option<&PathBuf>,
        branch: Option<&str>,
    ) -> Result<Option<PathBuf>, String> {
        if let Some(path) = self.checkout_path_from_key(checkout_key) {
            return Ok(Some(path));
        }

        match branch {
            Some(branch_name) => {
                let checkout_manager = self
                    .read_only
                    .registry
                    .checkout_managers
                    .preferred()
                    .cloned()
                    .ok_or_else(|| "No checkout manager available".to_string())?;
                let ee_root = ExecutionEnvironmentPath::new(self.repo_root);
                let (path, _checkout) = checkout_manager.create_checkout(&ee_root, branch_name, false).await?;
                Ok(Some(path.into_path_buf()))
            }
            None => Ok(None),
        }
    }

    pub(super) async fn create_workspace_for_teleport(
        &self,
        checkout_path: &Path,
        branch: Option<&str>,
        teleport_cmd: &str,
    ) -> Result<(), String> {
        let workspace_orchestrator = WorkspaceOrchestrator::new(
            self.repo_root,
            self.read_only.registry,
            self.config_base,
            self.attachable_store,
            self.daemon_socket_path,
            self.local_host,
            self.terminal_manager,
        );
        let name = branch.unwrap_or("session");
        workspace_orchestrator.create_workspace_for_teleport(checkout_path, name, teleport_cmd).await
    }

    fn checkout_path_from_key(&self, checkout_key: Option<&PathBuf>) -> Option<PathBuf> {
        checkout_key.and_then(|key| {
            let host_key = flotilla_protocol::HostPath::new(self.local_host.clone(), key.clone());
            self.read_only.providers_data.checkouts.get(&host_key).map(|_| key.clone())
        })
    }
}

impl<'a> TeleportFlow<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        repo_root: &'a Path,
        registry: &'a ProviderRegistry,
        providers_data: &'a ProviderData,
        config_base: &'a Path,
        attachable_store: &'a SharedAttachableStore,
        daemon_socket_path: Option<&'a Path>,
        local_host: &'a HostName,
        _session_id: &'a str,
        _branch: Option<&'a str>,
        checkout_key: Option<&'a PathBuf>,
    ) -> Self {
        Self {
            service: TeleportSessionActionService::new(
                repo_root,
                registry,
                providers_data,
                config_base,
                attachable_store,
                daemon_socket_path,
                local_host,
                None,
            ),
            checkout_key,
        }
    }

    pub(super) async fn initial_checkout_path(&self) -> Result<Option<PathBuf>, String> {
        self.service.resolve_teleport_checkout_path(self.checkout_key, None).await
    }
}

fn session_provider_key<'a>(session: &'a CloudAgentSession, session_id: &str) -> Option<&'a str> {
    session.correlation_keys.iter().find_map(|key| match key {
        CorrelationKey::SessionRef(provider, id) if id == session_id => Some(provider.as_str()),
        _ => None,
    })
}

pub(super) async fn resolve_attach_command(
    session_id: &str,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
) -> Result<String, String> {
    let provider_key = providers_data
        .sessions
        .get(session_id)
        .and_then(|session| session_provider_key(session, session_id))
        .ok_or_else(|| format!("Cannot determine provider for session {session_id}"))?;

    let (_, coding_agent) = registry.cloud_agents.get(provider_key).ok_or_else(|| format!("No coding agent provider: {provider_key}"))?;

    coding_agent.attach_command(session_id).await
}
