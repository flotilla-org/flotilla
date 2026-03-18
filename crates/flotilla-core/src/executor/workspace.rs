use std::{path::Path, sync::Arc};

use flotilla_protocol::{AttachableSetId, HostName, HostPath, PreparedTerminalCommand};
use tracing::{info, warn};

use super::{local_workspace_directory, terminals::TerminalPreparationService, workspace_config};
use crate::{
    attachable::{BindingObjectKind, ProviderBinding, SharedAttachableStore},
    providers::{registry::ProviderRegistry, workspace::WorkspaceManager},
    step::StepOutcome,
};

pub(super) struct WorkspaceOrchestrator<'a> {
    repo_root: &'a Path,
    registry: &'a ProviderRegistry,
    config_base: &'a Path,
    attachable_store: &'a SharedAttachableStore,
    daemon_socket_path: Option<&'a Path>,
    local_host: &'a HostName,
}

impl<'a> WorkspaceOrchestrator<'a> {
    pub(super) fn new(
        repo_root: &'a Path,
        registry: &'a ProviderRegistry,
        config_base: &'a Path,
        attachable_store: &'a SharedAttachableStore,
        daemon_socket_path: Option<&'a Path>,
        local_host: &'a HostName,
    ) -> Self {
        Self { repo_root, registry, config_base, attachable_store, daemon_socket_path, local_host }
    }

    pub(super) async fn create_workspace_for_checkout(&self, checkout_path: &Path, label: &str) -> Result<StepOutcome, String> {
        let Some((provider_name, ws_mgr)) = self.preferred_workspace_manager() else {
            return Ok(StepOutcome::Skipped);
        };

        if self.select_existing_workspace(ws_mgr.as_ref(), checkout_path).await {
            return Ok(StepOutcome::Completed);
        }

        let terminal_preparation =
            TerminalPreparationService::new(self.registry, self.config_base, self.attachable_store, self.daemon_socket_path);
        let mut config = workspace_config(self.repo_root, label, checkout_path, "claude", self.config_base);
        terminal_preparation.resolve_workspace_commands(&mut config).await;

        match ws_mgr.create_workspace(&config).await {
            Ok((ws_ref, _workspace)) => {
                self.persist_workspace_binding(provider_name, &ws_ref, self.local_host, checkout_path);
                Ok(StepOutcome::Completed)
            }
            Err(err) => Err(err),
        }
    }

    pub(super) async fn create_workspace_for_teleport(&self, checkout_path: &Path, label: &str, teleport_cmd: &str) -> Result<(), String> {
        let Some((provider_name, ws_mgr)) = self.preferred_workspace_manager() else {
            return Ok(());
        };

        let terminal_preparation =
            TerminalPreparationService::new(self.registry, self.config_base, self.attachable_store, self.daemon_socket_path);
        let mut config = workspace_config(self.repo_root, label, checkout_path, teleport_cmd, self.config_base);
        terminal_preparation.resolve_workspace_commands(&mut config).await;

        match ws_mgr.create_workspace(&config).await {
            Ok((ws_ref, _workspace)) => {
                self.persist_workspace_binding(provider_name, &ws_ref, self.local_host, checkout_path);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub(super) async fn create_workspace_from_prepared_terminal(
        &self,
        target_host: &HostName,
        branch: &str,
        checkout_path: &Path,
        attachable_set_id: Option<&AttachableSetId>,
        commands: &[PreparedTerminalCommand],
    ) -> Result<(), String> {
        let Some((provider_name, ws_mgr)) = self.preferred_workspace_manager() else {
            return Ok(());
        };

        let terminal_preparation =
            TerminalPreparationService::new(self.registry, self.config_base, self.attachable_store, self.daemon_socket_path);
        let wrapped = terminal_preparation.wrap_remote_attach_commands(target_host, checkout_path, commands)?;

        // The workspace itself is local to the presentation host, so its
        // working directory only needs to be a valid local directory.
        // The wrapped attach commands handle entering the remote checkout path.
        let working_dir = local_workspace_directory(self.repo_root, self.config_base);
        let remote_name = format!("{branch}@{target_host}");
        let mut config = workspace_config(self.repo_root, &remote_name, &working_dir, "claude", self.config_base);
        config.resolved_commands = Some(wrapped.into_iter().map(|cmd| (cmd.role, cmd.command)).collect());

        match ws_mgr.create_workspace(&config).await {
            Ok((ws_ref, _workspace)) => {
                if let Some(set_id) = attachable_set_id {
                    self.persist_workspace_binding_for_set(provider_name, &ws_ref, set_id, target_host, checkout_path);
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub(super) fn ensure_attachable_set_for_checkout(&self, target_host: &HostName, checkout_path: &Path) -> Option<AttachableSetId> {
        let Ok(mut store) = self.attachable_store.lock() else {
            warn!("attachable store lock poisoned while ensuring attachable set for checkout");
            return None;
        };

        let checkout = HostPath::new(target_host.clone(), checkout_path.to_path_buf());
        let (set_id, changed) = store.ensure_terminal_set_with_change(Some(target_host.clone()), Some(checkout));
        if changed {
            if let Err(err) = store.save() {
                warn!(err = %err, "failed to persist attachable registry after ensuring attachable set");
            }
        }
        Some(set_id)
    }

    pub(super) async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        if let Some(ws_mgr) = self.registry.workspace_managers.preferred() {
            ws_mgr.select_workspace(ws_ref).await?;
        }
        Ok(())
    }

    fn preferred_workspace_manager(&self) -> Option<(&str, &Arc<dyn WorkspaceManager>)> {
        self.registry.workspace_managers.preferred_with_desc().map(|(desc, provider)| (desc.implementation.as_str(), provider))
    }

    async fn select_existing_workspace(&self, ws_mgr: &dyn WorkspaceManager, checkout_path: &Path) -> bool {
        let existing = match ws_mgr.list_workspaces().await {
            Ok(workspaces) => workspaces,
            Err(err) => {
                warn!(err = %err, "failed to check existing workspaces, will create new");
                return false;
            }
        };

        for (ws_ref, ws) in &existing {
            if ws.directories.iter().any(|directory| directory == checkout_path) {
                info!(%ws_ref, path = %checkout_path.display(), "workspace already exists, selecting");
                if let Err(err) = ws_mgr.select_workspace(ws_ref).await {
                    warn!(err = %err, %ws_ref, "failed to select existing workspace, will create new");
                    return false;
                }
                return true;
            }
        }

        false
    }

    fn persist_workspace_binding(&self, provider_name: &str, workspace_ref: &str, target_host: &HostName, checkout_path: &Path) {
        let Ok(mut store) = self.attachable_store.lock() else {
            warn!("attachable store lock poisoned while persisting workspace binding");
            return;
        };

        let (set_id, changed_set) = store.ensure_terminal_set_with_change(
            Some(target_host.clone()),
            Some(HostPath::new(target_host.clone(), checkout_path.to_path_buf())),
        );
        let changed_binding = store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: provider_name.to_string(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: workspace_ref.to_string(),
        });
        if changed_set || changed_binding {
            if let Err(err) = store.save() {
                warn!(err = %err, "failed to persist attachable registry after workspace binding update");
            }
        }
    }

    fn persist_workspace_binding_for_set(
        &self,
        provider_name: &str,
        workspace_ref: &str,
        set_id: &AttachableSetId,
        target_host: &HostName,
        checkout_path: &Path,
    ) {
        let Ok(mut store) = self.attachable_store.lock() else {
            warn!("attachable store lock poisoned while persisting workspace binding");
            return;
        };

        if !store.registry().sets.contains_key(set_id) {
            store.insert_set(flotilla_protocol::AttachableSet {
                id: set_id.clone(),
                host_affinity: Some(target_host.clone()),
                checkout: Some(HostPath::new(target_host.clone(), checkout_path.to_path_buf())),
                template_identity: None,
                members: Vec::new(),
            });
        }
        let changed_binding = store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: provider_name.to_string(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: workspace_ref.to_string(),
        });
        if changed_binding {
            if let Err(err) = store.save() {
                warn!(err = %err, "failed to persist attachable registry after workspace binding update");
            }
        }
    }
}
