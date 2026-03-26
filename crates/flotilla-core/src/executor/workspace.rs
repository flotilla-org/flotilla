use std::{collections::HashMap, path::Path, sync::Arc};

use flotilla_protocol::{arg, AttachableSetId, HostName, HostPath, PreparedWorkspace, ResolvedPaneCommand};
use tracing::{info, warn};

use super::{terminals::TerminalPreparationService, workspace_config};
use crate::{
    attachable::{BindingObjectKind, ProviderBinding, SharedAttachableStore},
    hop_chain::{
        builder::HopPlanBuilder,
        environment::NoopEnvironmentHopResolver,
        remote::ssh_resolver_from_config,
        resolver::{AlwaysWrap, HopResolver},
        terminal::NoopTerminalHopResolver,
        ResolutionContext, ResolvedAction,
    },
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{registry::ProviderRegistry, types::WorkspaceAttachRequest, workspace::WorkspaceManager},
    terminal_manager::TerminalManager,
};

pub(super) struct WorkspaceOrchestrator<'a> {
    repo_root: &'a Path,
    registry: &'a ProviderRegistry,
    config_base: &'a Path,
    attachable_store: &'a SharedAttachableStore,
    daemon_socket_path: Option<&'a Path>,
    local_host: &'a HostName,
    terminal_manager: Option<&'a TerminalManager>,
}

impl<'a> WorkspaceOrchestrator<'a> {
    pub(super) fn new(
        repo_root: &'a Path,
        registry: &'a ProviderRegistry,
        config_base: &'a Path,
        attachable_store: &'a SharedAttachableStore,
        daemon_socket_path: Option<&'a Path>,
        local_host: &'a HostName,
        terminal_manager: Option<&'a TerminalManager>,
    ) -> Self {
        Self { repo_root, registry, config_base, attachable_store, daemon_socket_path, local_host, terminal_manager }
    }

    pub(super) async fn create_workspace_for_teleport(&self, checkout_path: &Path, label: &str, teleport_cmd: &str) -> Result<(), String> {
        let Some((provider_name, ws_mgr)) = self.preferred_workspace_manager() else {
            return Ok(());
        };

        let mut config = workspace_config(self.repo_root, label, checkout_path, teleport_cmd, self.config_base);
        if let Some(tm) = self.terminal_manager {
            let terminal_preparation = TerminalPreparationService::new(tm, self.daemon_socket_path);
            terminal_preparation.resolve_workspace_commands(&mut config).await;
        }
        let attach_request = workspace_attach_request_from_config(config);

        match ws_mgr.create_workspace(&attach_request).await {
            Ok((ws_ref, _workspace)) => {
                self.persist_workspace_binding(provider_name, &ws_ref, self.local_host, checkout_path);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub(super) async fn attach_prepared_workspace(&self, prepared: &PreparedWorkspace) -> Result<(), String> {
        let Some((provider_name, ws_mgr)) = self.preferred_workspace_manager() else {
            return Ok(());
        };

        if prepared.target_host == *self.local_host && self.select_existing_workspace(ws_mgr.as_ref(), &prepared.checkout_path).await {
            return Ok(());
        }

        let attach_commands = resolve_prepared_commands_via_hop_chain(
            &prepared.target_host,
            &prepared.checkout_path,
            &prepared.prepared_commands,
            self.config_base,
            self.local_host,
        )?;

        // The workspace itself is local to the presentation host, so its
        // working directory only needs to be a valid local directory.
        // The resolved commands handle entering the remote checkout path.
        // For remote-only repos (synthetic path like "<remote>/..."), fall
        // back to the user's home or cwd since the path doesn't exist locally.
        let working_dir = if self.repo_root.exists() {
            ExecutionEnvironmentPath::new(self.repo_root)
        } else if let Some(home) = dirs::home_dir() {
            ExecutionEnvironmentPath::new(home)
        } else if let Ok(cwd) = std::env::current_dir() {
            ExecutionEnvironmentPath::new(cwd)
        } else {
            ExecutionEnvironmentPath::new(self.config_base)
        };
        let attach_request = WorkspaceAttachRequest {
            name: prepared.label.clone(),
            working_directory: working_dir,
            template_vars: HashMap::from([("main_command".to_string(), "claude".to_string())]),
            template_yaml: prepared.template_yaml.clone(),
            attach_commands,
        };

        match ws_mgr.create_workspace(&attach_request).await {
            Ok((ws_ref, _workspace)) => {
                if let Some(set_id) = prepared.attachable_set_id.as_ref() {
                    self.persist_workspace_binding_for_set(provider_name, &ws_ref, set_id, &prepared.target_host, &prepared.checkout_path);
                } else {
                    self.persist_workspace_binding(provider_name, &ws_ref, &prepared.target_host, &prepared.checkout_path);
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
                environment_id: None,
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

fn workspace_attach_request_from_config(config: crate::providers::types::WorkspaceConfig) -> WorkspaceAttachRequest {
    WorkspaceAttachRequest {
        name: config.name,
        working_directory: config.working_directory,
        template_vars: config.template_vars,
        template_yaml: config.template_yaml,
        attach_commands: config.resolved_commands.unwrap_or_default(),
    }
}

/// Resolve prepared pane commands through the hop chain, producing `(role, command_string)` pairs
/// suitable for workspace manager consumption.
///
/// For each `ResolvedPaneCommand`, builds a `HopPlan` via `HopPlanBuilder::build_for_prepared_command`,
/// resolves it with `SshRemoteHopResolver` + `AlwaysWrap`, and flattens the resulting `Command` to a string.
fn resolve_prepared_commands_via_hop_chain(
    target_host: &HostName,
    checkout_path: &Path,
    commands: &[ResolvedPaneCommand],
    config_base: &Path,
    local_host: &HostName,
) -> Result<Vec<(String, String)>, String> {
    let ssh_resolver = ssh_resolver_from_config(&DaemonHostPath::new(config_base))?;
    let hop_resolver = HopResolver {
        remote: Arc::new(ssh_resolver),
        environment: Arc::new(NoopEnvironmentHopResolver),
        terminal: Arc::new(NoopTerminalHopResolver),
        strategy: Arc::new(AlwaysWrap),
    };
    let plan_builder = HopPlanBuilder::new(local_host);

    let mut result = Vec::with_capacity(commands.len());
    for cmd in commands {
        let plan = plan_builder.build_for_prepared_command(target_host, &cmd.args);
        let mut context = ResolutionContext {
            current_host: local_host.clone(),
            current_environment: None,
            working_directory: Some(ExecutionEnvironmentPath::new(checkout_path)),
            actions: Vec::new(),
            nesting_depth: 0,
        };
        let resolved = hop_resolver.resolve(&plan, &mut context)?;

        // AlwaysWrap should produce exactly one Command action. Assert this invariant
        // so multi-action plans don't silently lose actions.
        if resolved.0.len() != 1 {
            return Err(format!(
                "hop chain resolution produced {} actions for role '{}', expected exactly 1 (AlwaysWrap)",
                resolved.0.len(),
                cmd.role
            ));
        }
        let command_string = match resolved.0.into_iter().next() {
            Some(ResolvedAction::Command(args)) => arg::flatten(&args, 0),
            Some(_) => return Err(format!("hop chain resolution produced a non-Command action for role '{}'", cmd.role)),
            None => unreachable!("len checked above"),
        };

        result.push((cmd.role.clone(), command_string));
    }
    Ok(result)
}
