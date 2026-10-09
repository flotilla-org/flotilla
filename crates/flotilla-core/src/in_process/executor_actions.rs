//! Provider execution, repository administration, and resource mutation commands.
//!
//! Handlers use the capability-owned port below; the composition root supplies
//! orchestration collaborators and shares their existing state.
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::commands::RepositoryIdentityChange;
use flotilla_protocol::Command;
use flotilla_protocol::CommandAction;
use flotilla_protocol::CommandValue;
use flotilla_protocol::DaemonEvent;
use flotilla_protocol::HostName;
use flotilla_protocol::IssueRef;
use flotilla_protocol::IssueSource;
use flotilla_protocol::NodeId;
use flotilla_protocol::PeerConnectionState;
use flotilla_protocol::ProviderData;
use flotilla_protocol::RepoIdentity;
use flotilla_protocol::RepoSelector;
use flotilla_protocol::ResourceJsonResponse;
use flotilla_resources::Checkout as ResourceCheckout;
use flotilla_resources::DynamicResourceObject;
use flotilla_resources::InputMeta;
use flotilla_resources::Repository;
use flotilla_resources::RepositoryKey;
use flotilla_resources::Resource;
use flotilla_resources::ResourceBackend;
use flotilla_resources::ResourceError;
use flotilla_resources::ResourceObject;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{
    empty_repo_identity, fallback_repo_identity, repository_operations, request_manifest_resolution, retry_resource_apply, AddRepoOutcome,
    OperatorReconciler,
};
use crate::config::ConfigStore;
use crate::environment_manager::EnvironmentManager;
use crate::event_sink::EventSink;
use crate::executor;
use crate::providers::discovery::EnvVars;
use crate::providers::registry::ProviderRegistry;
use crate::providers::CommandRunner;
use crate::step::run_step_plan_with_remote_executor;
use crate::step::RemoteStepExecutor;

#[async_trait]
pub(super) trait ExecutorActionPort: Send + Sync {
    async fn post_install_cleat(
        &self,
        incoming: &Path,
        generation: &str,
        diagnostics_dir: &Path,
    ) -> Result<crate::cleat_roll::RollReport, String>;
    fn vcs_resolver(&self) -> Result<Arc<dyn crate::vcs::CheckoutVcsResolver>, String>;
    async fn resolve_repo_for_command(&self, command: &Command) -> Result<PathBuf, String>;
    async fn repository_registry(&self, repository: &ResourceObject<Repository>) -> Result<Arc<ProviderRegistry>, String>;
    fn host_name(&self) -> &HostName;
    async fn executor_provider_data(&self, repo_identity: &RepoIdentity, _repo_root: &Path, registry: &ProviderRegistry) -> ProviderData;
    async fn execution_registry(&self, repository: &ResourceObject<Repository>, path: &Path) -> Result<Arc<ProviderRegistry>, String>;
    fn environment_manager(&self) -> &Arc<EnvironmentManager>;
    fn runner(&self) -> &Arc<dyn CommandRunner>;
    fn env(&self) -> &Arc<dyn EnvVars>;
    fn daemon_socket_path(&self) -> &RwLock<Option<PathBuf>>;
    fn config(&self) -> &Arc<ConfigStore>;
    fn active_commands(&self) -> &Arc<Mutex<HashMap<u64, CancellationToken>>>;
    async fn add_repo(&self, path: &Path) -> Result<AddRepoOutcome, String>;
    async fn apply_intent_document(&self, namespace: &str, document: serde_json::Value) -> Result<DynamicResourceObject, ResourceError>;
    async fn detect_repo_identity(&self, repo_path: &Path) -> RepoIdentity;
    fn event_sink(&self) -> &Arc<dyn EventSink>;
    fn finish_context_free_command(&self, command_id: u64, repo_identity: RepoIdentity, result: CommandValue);
    async fn local_checkout_for_repository(&self, key: &RepositoryKey) -> Result<Option<PathBuf>, String>;
    fn node_id(&self) -> &NodeId;
    fn observed_checkout_reconciliation(&self) -> &Arc<Mutex<()>>;
    fn observed_resource_backend(&self) -> &ResourceBackend;
    async fn operator_reconciler(&self) -> Option<Arc<dyn OperatorReconciler>>;
    async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState;
    async fn provisioning_namespace(&self) -> String;
    async fn refresh(&self, repo: &RepoSelector) -> Result<Option<RepositoryIdentityChange>, String>;
    async fn remove_repo(&self, path: &Path) -> Result<(), String>;
    async fn repository_for_selector(&self, selector: &RepoSelector) -> Result<ResourceObject<Repository>, String>;
    fn resolve_observation_root_selector(&self, selector: &RepoSelector) -> Result<PathBuf, String>;
    async fn resolve_repo_selector(&self, selector: &RepoSelector) -> Result<PathBuf, String>;
    fn resource_backend(&self) -> &ResourceBackend;
    fn start_context_free_command(&self, command_id: u64, description: String) -> RepoIdentity;
    async fn tracked_repo_identity_for_path(&self, repo_path: &Path) -> Option<RepoIdentity>;
}

pub(super) struct ExecutorActions<'a> {
    pub(super) port: &'a dyn ExecutorActionPort,
}

impl ExecutorActions<'_> {
    pub(super) async fn execute_action_resource_apply(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ResourceApply { namespace, document } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            // Artifact reservations and Message admission can race status writers.
            // Both mutations are replay-safe; retain a bounded conflict budget.
            let kind = document.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
            let applied = retry_resource_apply(kind, || self.port.apply_intent_document(namespace, document.clone())).await;
            let result = match applied {
                Ok(applied) => CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                    kind: applied.kind,
                    plural: applied.plural,
                    namespace: applied.namespace,
                    value: applied.value,
                    replica_origin: None,
                })),
                Err(error) => CommandValue::Error { message: error.to_string() },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ResourceApply action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_repository_remote_remove(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::RepositoryRemoteRemove { namespace, name, remote } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let repositories = self.port.resource_backend().clone().using::<Repository>(namespace);
            let result = match repositories.get(name).await {
                Ok(repository) => match repository.spec.clone().remove_remote(remote) {
                    Ok(spec) => match repositories
                        .update(&InputMeta::from(&repository.metadata), &repository.metadata.resource_version, &spec)
                        .await
                    {
                        Ok(_) => CommandValue::Ok,
                        Err(error) => CommandValue::Error { message: error.to_string() },
                    },
                    Err(message) => CommandValue::Error { message },
                },
                Err(error) => CommandValue::Error { message: error.to_string() },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("RepositoryRemoteRemove action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_resource_manifest_resolve(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ResourceManifestResolve { namespace, kind, name, resolution, requested_by } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = request_manifest_resolution(self.port.resource_backend(), namespace, kind, name, *resolution, requested_by).await;
            self.port.finish_context_free_command(
                id,
                empty_identity,
                match result {
                    Ok(root) => CommandValue::ResourceObject(Box::new(root)),
                    Err(error) => CommandValue::Error { message: error },
                },
            );
            return Ok(id);
        }
        Err("ResourceManifestResolve action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_resource_reconcile_now(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ResourceReconcileNow { namespace, kind, name } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = match self.port.operator_reconciler().await {
                Some(reconciler) => match reconciler.reconcile_now(namespace, kind, name).await {
                    Ok(message) => CommandValue::ResourceReconciled { resource_kind: kind.clone(), name: name.clone(), message },
                    Err(message) => CommandValue::Error { message },
                },
                None => CommandValue::Error { message: "operator reconciliation is unavailable before runtime startup".to_string() },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ResourceReconcileNow action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_resource_status_patch(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ResourceStatusPatch { namespace, kind, name, status, expected_resource_version } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let patched = match expected_resource_version {
                Some(expected) => {
                    flotilla_resources::patch_resource_status_if_version(
                        self.port.resource_backend(),
                        namespace,
                        kind,
                        name,
                        status.clone(),
                        expected,
                    )
                    .await
                }
                None => {
                    flotilla_resources::patch_resource_status(self.port.resource_backend(), namespace, kind, name, status.clone()).await
                }
            };
            let result = match patched {
                Ok(patched) => CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                    kind: patched.kind,
                    plural: patched.plural,
                    namespace: patched.namespace,
                    value: patched.value,
                    replica_origin: None,
                })),
                Err(error) => CommandValue::Error { message: error.to_string() },
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ResourceStatusPatch action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_resource_delete(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::ResourceDelete { namespace, kind, name, replica_origin } = &command.action {
            let empty_identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = if let Some(origin_root) = replica_origin {
                let deleted = if self.port.peer_connection_status(origin_root).await == PeerConnectionState::Connected {
                    Err(ResourceError::invalid(format!(
                        "replica origin {origin_root} is connected; delete the authoritative resource instead"
                    )))
                } else {
                    flotilla_resources::collect_resource_replica_kind(self.port.resource_backend(), namespace, kind, name, origin_root)
                        .await
                };
                match deleted {
                    Ok(deleted) => CommandValue::ResourceDeleted(Box::new(ResourceJsonResponse {
                        kind: deleted.kind,
                        plural: deleted.plural,
                        namespace: deleted.namespace,
                        value: deleted.value,
                        replica_origin: replica_origin.clone(),
                    })),
                    Err(error) => CommandValue::Error { message: error.to_string() },
                }
            } else {
                // Serialize deletion and cleanup with adopted checkout writes.
                let _reconciliation = self.port.observed_checkout_reconciliation().lock().await;
                let deleted = async {
                    let deleted = flotilla_resources::delete_resource_kind(self.port.resource_backend(), namespace, kind, name).await?;
                    if deleted.object.kind == ResourceCheckout::API_PATHS.kind {
                        crate::observed_resources::delete_stale_adopted_checkouts(
                            self.port.resource_backend(),
                            self.port.observed_resource_backend(),
                            namespace,
                        )
                        .await?;
                    }
                    Ok::<_, ResourceError>(deleted)
                }
                .await;
                match deleted {
                    Ok(deleted) => {
                        let response = Box::new(ResourceJsonResponse {
                            kind: deleted.object.kind,
                            plural: deleted.object.plural,
                            namespace: deleted.object.namespace,
                            value: deleted.object.value,
                            replica_origin: None,
                        });
                        if deleted.already_deleted {
                            CommandValue::ResourceAlreadyDeleted(response)
                        } else {
                            CommandValue::ResourceDeleted(response)
                        }
                    }
                    Err(error) => CommandValue::Error { message: error.to_string() },
                }
            };
            self.port.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("ResourceDelete action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_refresh_all(&self, id: u64, command: &Command) -> Result<u64, String> {
        if matches!(command.action, CommandAction::Refresh { repo: None }) {
            let repositories = self
                .port
                .resource_backend()
                .including_replicas::<Repository>(&self.port.provisioning_namespace().await)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items;
            let repo_identity = empty_repo_identity();
            let description = command.description().to_string();
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: repo_identity.clone(),
                repo: None,
                description,
            });
            let mut refreshed = Vec::new();
            let mut identity_changes = Vec::new();
            let result = match async {
                for repository in &repositories {
                    let key = repository.object.spec.key();
                    if let Some(change) = self.port.refresh(&RepoSelector::Repository(key.clone())).await? {
                        identity_changes.push(change);
                    }
                    if let Some(path) = self.port.local_checkout_for_repository(&key).await? {
                        refreshed.push(path);
                    }
                }
                Ok::<(), String>(())
            }
            .await
            {
                Ok(()) => CommandValue::Refreshed { repos: refreshed, repository_count: repositories.len(), identity_changes },
                Err(message) => CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("Refresh action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_track_repo_path(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::TrackRepoPath { path } = &command.action {
            let description = command.description().to_string();
            let repo_path = path.clone();
            let repo_identity = self.port.detect_repo_identity(path).await;
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.port.add_repo(path).await {
                Ok(outcome) => CommandValue::RepoTracked {
                    path: outcome.tracked_path,
                    resolved_from: outcome.resolved_from,
                    identity_change: outcome.identity_change,
                },
                Err(message) => CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: self.port.tracked_repo_identity_for_path(path).await.unwrap_or(repo_identity),
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }
        Err("TrackRepoPath action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_untrack_repo(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::UntrackRepo { repo } = &command.action {
            let repo_path = match self.port.resolve_repo_selector(repo).await {
                Ok(path) => path,
                Err(tracked_error) => self.port.resolve_observation_root_selector(repo).map_err(|_| tracked_error)?,
            };
            let description = command.description().to_string();
            let repo_identity =
                self.port.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.port.remove_repo(&repo_path).await {
                Ok(()) => CommandValue::RepoUntracked { path: repo_path.clone() },
                Err(message) => CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }
        Err("UntrackRepo action selected the wrong handler".to_string())
    }

    pub(super) async fn execute_action_refresh_repo(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::Refresh { repo: Some(selector) } = &command.action {
            let repository = self.port.repository_for_selector(selector).await?;
            let repo_path = self.port.local_checkout_for_repository(&repository.spec.key()).await?;
            let description = command.description().to_string();
            let repo_identity = repository_operations::repository_event_identity(&repository.spec, repo_path.as_deref());
            self.port.event_sink().emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity: repo_identity.clone(),
                repo: repo_path.clone(),
                description,
            });
            let result = match self.port.refresh(selector).await {
                Ok(identity_change) => CommandValue::Refreshed {
                    repository_count: 1,
                    repos: repo_path.clone().into_iter().collect(),
                    identity_changes: identity_change.into_iter().collect(),
                },
                Err(message) => CommandValue::Error { message },
            };
            self.port.event_sink().emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.port.node_id().clone(),
                repo_identity,
                repo: repo_path,
                result,
            });
            return Ok(id);
        }
        Err("Refresh action selected the wrong handler".to_string())
    }
}

impl ExecutorActions<'_> {
    pub(super) async fn execute_provider_action(
        &self,
        id: u64,
        command: Command,
        command_node_id: NodeId,
        remote_executor: Arc<dyn RemoteStepExecutor>,
    ) -> Result<u64, String> {
        // Gather what the spawned task needs — validate repo before broadcasting
        let repo = self.port.resolve_repo_for_command(&command).await?;
        let runner = Arc::clone(self.port.runner());
        let env = Arc::clone(self.port.env());
        let event_sink = self.port.event_sink().clone();
        let repository = self.port.repository_for_selector(&RepoSelector::Path(repo.clone())).await?;
        let repo_identity = repository_operations::repository_event_identity(&repository.spec, None);
        let registry = self.port.execution_registry(&repository, &repo).await?;
        let providers_data = Arc::new(self.port.executor_provider_data(&repo_identity, &repo, &registry).await);

        let description = command.description().to_string();
        let repo_path = repo.to_path_buf();
        let config_base = DaemonHostPath::new(self.port.config().base_path().as_path());

        let active_ref = Arc::clone(self.port.active_commands());
        let token = CancellationToken::new();
        {
            let mut guard = active_ref.lock().await;
            guard.insert(id, token.clone());
        }

        self.port.event_sink().emit(DaemonEvent::CommandStarted {
            command_id: id,
            node_id: command_node_id.clone(),
            repo_identity: repo_identity.clone(),
            repo: Some(repo_path.clone()),
            description,
        });

        let local_host = self.port.host_name().clone();
        let local_node_id = self.port.node_id().clone();
        let daemon_socket_path = self.port.daemon_socket_path().read().await.clone();
        let environment_manager = Arc::clone(self.port.environment_manager());
        let vcs_resolver = self.port.vcs_resolver()?;
        tokio::spawn(async move {
            let resolver_registry = Arc::clone(&registry);
            let resolver_providers_data = Arc::clone(&providers_data);
            let resolver_runner = Arc::clone(&runner);
            let resolver_env = Arc::clone(&env);
            let resolver_config_base = config_base.clone();
            let resolver_local_host = local_host.clone();
            let ee_repo_path = ExecutionEnvironmentPath::new(&repo_path);
            let resolver_repo = executor::RepoExecutionContext { identity: repo_identity.clone(), root: ee_repo_path.clone() };
            let daemon_socket_dhp = daemon_socket_path.map(DaemonHostPath::new);

            let plan = executor::build_plan(command, providers_data, local_node_id.clone(), local_host)
                .await
                .map_err(executor::PlannerRefusal::into_command_value);

            match plan {
                Err(result) => {
                    {
                        let mut guard = active_ref.lock().await;
                        guard.remove(&id);
                    }
                    event_sink.emit(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: command_node_id.clone(),
                        repo_identity: repo_identity.clone(),
                        repo: Some(repo_path),
                        result,
                    });
                }
                Ok(step_plan) => {
                    let resolver = executor::ExecutorStepResolver {
                        repo: resolver_repo,
                        registry: resolver_registry,
                        providers_data: resolver_providers_data,
                        runner: resolver_runner,
                        env: resolver_env,
                        config_base: resolver_config_base,
                        daemon_socket_path: daemon_socket_dhp.clone(),

                        local_host: resolver_local_host.clone(),
                        environment_manager: Arc::clone(&environment_manager),
                        vcs_resolver: Arc::clone(&vcs_resolver),
                    };
                    let result = run_step_plan_with_remote_executor(
                        step_plan,
                        id,
                        local_node_id,
                        repo_identity.clone(),
                        ExecutionEnvironmentPath::new(&repo_path),
                        token,
                        event_sink.clone(),
                        &resolver,
                        remote_executor.as_ref(),
                    )
                    .await;
                    let mut guard = active_ref.lock().await;
                    guard.remove(&id);
                    event_sink.emit(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: command_node_id,
                        repo_identity,
                        repo: Some(repo_path),
                        result,
                    });
                }
            }
        });

        Ok(id)
    }
    pub(super) async fn execute_action_repository_forge(&self, command_id: u64, command: &Command) -> Result<u64, String> {
        let selector = command.context_repo.as_ref().ok_or("command requires Repository context")?;
        let repository = self.port.repository_for_selector(selector).await?;
        if repository.spec.forge().is_none() {
            return Err("Repository has no forge identity".into());
        }
        let identity = repository_operations::repository_event_identity(&repository.spec, None);
        let registry = self.port.repository_registry(&repository).await?;
        let action = command.action.clone();
        let event_sink = self.port.event_sink().clone();
        let node_id = self.port.node_id().clone();
        let active_commands = Arc::clone(self.port.active_commands());
        let cancel = CancellationToken::new();
        active_commands.lock().await.insert(command_id, cancel.clone());
        event_sink.emit(DaemonEvent::CommandStarted {
            command_id,
            node_id: node_id.clone(),
            repo_identity: identity.clone(),
            repo: None,
            description: command.description().into(),
        });
        tokio::spawn(async move {
            let operation = async {
                if let CommandAction::OpenIssue { id } = &action {
                    let forge = repository.spec.issue_source_forge().ok_or("Repository has no forge issue source")?;
                    let source = IssueSource { service: forge.service_url, scope: forge.repository };
                    let provider = registry.issue_provider_for(&source).ok_or("no issue provider available for Repository")?;
                    return provider.open_in_browser(&IssueRef { source, id: id.clone() }).await;
                }
                if let CommandAction::MergeChangeRequest { id, confirmed } = &action {
                    if repository.spec.is_fork() {
                        return Err(format!("merging change request {id} is forbidden for fork-stance repository; landing is human-only"));
                    }
                    if !confirmed {
                        return Err(format!("merging change request {id} requires explicit confirmation"));
                    }
                }
                let provider = registry.change_requests.preferred().ok_or("no change request provider is active for this Repository")?;
                match action {
                    CommandAction::OpenChangeRequest { id } => provider.open_in_browser(&id).await,
                    CommandAction::CloseChangeRequest { id } => provider.close_change_request(&id).await,
                    CommandAction::MergeChangeRequest { id, .. } => provider.merge_change_request(&id).await,
                    CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
                        provider.link_issues(&change_request_id, &issue_ids).await
                    }
                    _ => Err("not a Repository forge action".into()),
                }
            };
            let result = tokio::select! {
                result = operation => match result { Ok(()) => CommandValue::Ok, Err(message) => CommandValue::Error { message } },
                _ = cancel.cancelled() => CommandValue::Cancelled,
            };
            active_commands.lock().await.remove(&command_id);
            event_sink.emit(DaemonEvent::CommandFinished { command_id, node_id, repo_identity: identity, repo: None, result });
        });
        Ok(command_id)
    }
}

impl ExecutorActions<'_> {
    pub(super) async fn execute_action_fleet_post_install(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let CommandAction::FleetPostInstall { cleat_bin, generation, diagnostics_dir } = &command.action {
            let identity = self.port.start_context_free_command(id, command.description().to_string());
            let result = match self.port.post_install_cleat(cleat_bin, generation, diagnostics_dir).await {
                Ok(report) => CommandValue::FleetPostInstall {
                    failed: report.failed(),
                    report: serde_json::to_value(report).map_err(|error| error.to_string())?,
                },
                Err(message) => CommandValue::Error { message },
            };
            self.port.finish_context_free_command(id, identity, result);
            return Ok(id);
        }

        Err("FleetPostInstall action selected the wrong handler".into())
    }
}
