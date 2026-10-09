//! Shared controller state and standing-convoy backing inspection.

use std::{
    collections::{BTreeSet, HashMap},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
};

use async_trait::async_trait;
use flotilla_controllers::reconcilers::clone::runtime::CloneFlights;
use flotilla_controllers::reconcilers::CheckoutRemoval;
use flotilla_core::{
    agent_process::ExitReceiptObserver,
    config::{ConfigStore, DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY},
    in_process::{InProcessDaemon, StandingConvoyBackingInspector},
    path_context::DaemonHostPath,
    providers::{registry::ProviderRegistry, ChannelLabel},
};
use flotilla_credentials::{AgentMaterialRegistry, CredentialStore};
use flotilla_protocol::{CanonicalHostId, EnvironmentId};
use flotilla_resources::{Convoy, ConvoyProvisioningState, Environment, Host, HostSpec, ResourceError, ResourceObject};
use tokio::sync::{Mutex, Semaphore};

use super::discovery::AgentlessSshProfile;
use super::environments::{stage_remote_rustc_wrapper, ActiveProvisionedEnvironment};
use super::tasks::{load_checkout_archive_roots, CheckoutArchiveRoot};
use super::terminal::PendingTerminalDelivery;
use crate::{
    blob_store::TieredBlobStore,
    environment_tools::{stage_local_rustc_wrapper_async, EnvironmentToolProvisioner},
};

pub(super) struct ControllerRuntimeState {
    pub(super) daemon: Arc<InProcessDaemon>,
    pub(super) config: Arc<ConfigStore>,
    pub(super) local_registry: Arc<ProviderRegistry>,
    pub(super) local_host_ref: String,
    pub(super) namespace: String,
    pub(super) host_direct_environment_name: String,
    pub(super) agentless_ssh: HashMap<String, AgentlessSshProfile>,
    pub(super) environment_tools: EnvironmentToolProvisioner,
    pub(super) credential_store: Option<Arc<CredentialStore>>,
    pub(super) agent_material: Option<Arc<AgentMaterialRegistry>>,
    pub(super) blob_store: Option<Arc<TieredBlobStore>>,
    pub(super) image_build_runner: Option<Arc<crate::image_build::BuildxRunner>>,
    pub(super) image_distributor: Option<Arc<crate::image_distribution::ImageDistributor<crate::image_distribution::DockerImageIo>>>,
    pub(super) provisioned_environments: Mutex<HashMap<String, ActiveProvisionedEnvironment>>,
    /// Latched after one complete post-startup local Docker adoption pass.
    /// A fresh provider listing is still required for each absence judgement.
    pub(super) local_backing_observed: AtomicBool,
    pub(super) clone_flights: Arc<CloneFlights>,
    pub(super) terminal_deliveries: StdMutex<HashMap<String, PendingTerminalDelivery>>,
    pub(super) exit_receipts: ExitReceiptObserver,
    pub(super) archive_catalog_lock: Mutex<()>,
    pub(super) checkout_removal_concurrency: NonZeroUsize,
    pub(super) checkout_removals: Semaphore,
}

impl ControllerRuntimeState {
    pub(super) async fn rust_build_jobs(&self, host_ref: &str) -> Result<usize, String> {
        let host_spec = match self.daemon.resource_backend().using::<Host>(&self.namespace).get(host_ref).await {
            Ok(host) => host.spec,
            Err(ResourceError::NotFound { .. }) if host_ref == self.local_host_ref => HostSpec::default(),
            Err(error) => return Err(format!("read build fulfilment setting for host {host_ref}: {error}")),
        };
        let cores = if let Some(profile) = self.agentless_ssh.values().find(|profile| profile.provisioning.host_id == host_ref) {
            profile
                .runner
                .run("getconf", &["_NPROCESSORS_ONLN"], Path::new("/"), &ChannelLabel::Default)
                .await?
                .trim()
                .parse::<usize>()
                .map_err(|error| format!("read core count for host {host_ref}: {error}"))?
        } else if host_ref == self.local_host_ref {
            std::thread::available_parallelism().map(usize::from).map_err(|error| format!("read local core count: {error}"))?
        } else {
            return Err(format!("no core count source for host {host_ref}"));
        };
        Ok(host_spec.rust_build_jobs(cores))
    }

    pub(super) async fn rustc_wrapper_for_environment(&self, env_ref: &str) -> Result<PathBuf, String> {
        if let Some(profile) = self.agentless_ssh.get(env_ref) {
            let base = profile.runner.writable_config_base(None, self.config.state_dir().as_path()).await?;
            return stage_remote_rustc_wrapper(profile.runner.as_ref(), &base).await;
        }
        stage_local_rustc_wrapper_async(self.config.state_dir().as_path().to_path_buf()).await
    }

    /// Construct an unstarted runtime with default limits. Consuming builders
    /// configure it before it is shared with controller tasks through `Arc`.
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        local_registry: Arc<ProviderRegistry>,
        daemon_socket_path: Option<DaemonHostPath>,
        local_host_ref: String,
        host_direct_environment_name: String,
    ) -> Self {
        let environment_tools = EnvironmentToolProvisioner::for_local_host(&daemon, &config, daemon_socket_path.clone());
        Self {
            daemon,
            config,
            local_registry,
            local_host_ref,
            namespace: flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE.to_string(),
            host_direct_environment_name,
            agentless_ssh: HashMap::new(),
            environment_tools,
            credential_store: None,
            agent_material: None,
            blob_store: None,
            image_build_runner: None,
            image_distributor: None,
            provisioned_environments: Mutex::new(HashMap::new()),
            local_backing_observed: AtomicBool::new(false),
            clone_flights: Arc::new(CloneFlights::default()),
            terminal_deliveries: StdMutex::new(HashMap::new()),
            exit_receipts: ExitReceiptObserver::default(),
            archive_catalog_lock: Mutex::new(()),
            checkout_removal_concurrency: DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY,
            checkout_removals: Semaphore::new(DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY.get()),
        }
    }

    pub(super) fn with_checkout_removal_concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.checkout_removal_concurrency = concurrency;
        self.checkout_removals = Semaphore::new(concurrency.get());
        self
    }

    pub(super) async fn register_checkout_archive_roots(&self, env_ref: &str, removal: &CheckoutRemoval) -> Result<(), String> {
        let (clone_path, target_path) = match removal {
            CheckoutRemoval::ForcedWorktree { clone_path, target_path, .. }
            | CheckoutRemoval::LandedWorktree { clone_path, target_path, .. } => (clone_path, target_path),
            _ => return Ok(()),
        };
        let _guard = self.archive_catalog_lock.lock().await;
        let catalog = self.config.state_dir().as_path().join("checkout-archive-roots.json");
        let mut roots = load_checkout_archive_roots(&catalog).await?;
        for source in [clone_path, target_path] {
            if let Some(parent) = Path::new(source).parent() {
                roots.insert(CheckoutArchiveRoot { env_ref: env_ref.to_string(), path: parent.join(".flotilla-archives") });
            }
        }
        tokio::fs::create_dir_all(catalog.parent().expect("catalog has parent"))
            .await
            .map_err(|error| format!("create checkout archive catalog parent: {error}"))?;
        let temporary = catalog.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec(&roots).map_err(|error| format!("encode checkout archive catalog: {error}"))?;
        let write_result = async {
            tokio::fs::write(&temporary, bytes).await.map_err(|error| format!("write checkout archive catalog: {error}"))?;
            tokio::fs::rename(&temporary, &catalog).await.map_err(|error| format!("replace checkout archive catalog: {error}"))
        }
        .await;
        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        write_result?;
        Ok(())
    }

    pub(super) fn with_agentless_ssh(mut self, profiles: Vec<AgentlessSshProfile>) -> Self {
        self.agentless_ssh = profiles.into_iter().map(|profile| (profile.environment_id.to_string(), profile)).collect();
        self
    }

    pub(super) fn with_namespace(mut self, namespace: String) -> Self {
        self.namespace = namespace;
        self
    }

    pub(super) fn agentless_host_refs(&self) -> Vec<CanonicalHostId> {
        self.agentless_ssh.values().map(|profile| CanonicalHostId::resolved(&profile.provisioning.host_id)).collect()
    }

    pub(super) fn with_credential_store(mut self, credential_store: Arc<CredentialStore>) -> Self {
        self.credential_store = Some(credential_store);
        self
    }

    pub(super) fn with_agent_material(mut self, agent_material: Arc<AgentMaterialRegistry>) -> Self {
        self.agent_material = Some(agent_material);
        self
    }

    pub(super) fn with_image_build_runner(mut self, runner: Arc<crate::image_build::BuildxRunner>) -> Self {
        self.image_distributor = runner.distributor.clone();
        self.image_build_runner = Some(runner);
        self
    }

    pub(super) fn with_blob_store(mut self, blob_store: Arc<TieredBlobStore>) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    #[cfg(test)]
    pub(super) fn with_environment_tools(mut self, environment_tools: EnvironmentToolProvisioner) -> Self {
        self.environment_tools = environment_tools;
        self
    }
}

pub(super) async fn canonical_runtime_host_id(
    daemon: &InProcessDaemon,
    namespace: &str,
    local_host_id: &CanonicalHostId,
    host_ref: &str,
) -> Result<CanonicalHostId, String> {
    if host_ref == local_host_id.as_str() {
        Ok(local_host_id.clone())
    } else {
        daemon.canonical_host_id_internal(namespace, host_ref).await
    }
}

#[async_trait]
impl StandingConvoyBackingInspector for ControllerRuntimeState {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        let environments = self
            .daemon
            .resource_backend()
            .using::<Environment>(&convoy.metadata.namespace)
            .list()
            .await
            .map_err(|error| format!("could not inspect backing environments: {error}"))?
            .items
            .into_iter()
            .filter(|environment| environment.metadata.labels.get(flotilla_resources::CONVOY_LABEL) == Some(&convoy.metadata.name))
            .collect::<Vec<_>>();
        if environments.is_empty() {
            if convoy.status.as_ref().and_then(|status| status.provisioning) == Some(ConvoyProvisioningState::NotStarted) {
                return Ok(());
            }
            let local_host_id = CanonicalHostId::resolved(&self.local_host_ref);
            let home = convoy
                .status
                .as_ref()
                .and_then(|status| status.placement_decision.as_ref())
                .map(|decision| &decision.target_host.reference);
            if home != Some(&local_host_id) || !self.local_backing_observed.load(Ordering::Acquire) {
                return Err("no backing environment evidence is available".to_string());
            }
            let providers = self
                .local_registry
                .environment_providers
                .iter()
                .filter(|(_, provider)| provider.kind() == flotilla_core::providers::environment::EnvironmentKind::Docker)
                .collect::<Vec<_>>();
            if providers.is_empty() {
                return Err("Docker environment provider absent for standing-convoy liveness check".into());
            }
            // Without surviving Environment records, inspect every instance before
            // claiming death. A matching backing on any endpoint prevents cleanup.
            let mut handles = Vec::new();
            for (_, provider) in providers {
                handles.extend(provider.list().await.map_err(|error| format!("Docker backing liveness check failed: {error}"))?);
            }
            let expected_environments = convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .map(|snapshot| {
                    snapshot
                        .vessels
                        .iter()
                        .map(|requirement| format!("env-{}-{}", convoy.metadata.name, requirement.name))
                        .collect::<BTreeSet<_>>()
                })
                .filter(|names| !names.is_empty())
                .ok_or_else(|| "backing is not verifiable: convoy has no frozen vessel names".to_string())?;
            if handles.iter().any(|handle| expected_environments.contains(handle.id().as_str())) {
                return Err("backing is not verified dead: matching Docker container remains".to_string());
            }
            return Ok(());
        }
        if let Some(environment) = environments.iter().find(|environment| environment.spec.docker.is_none()) {
            return Err(format!("Environment/{} has no inspectable Docker backing", environment.metadata.name));
        }
        let local_host_id = CanonicalHostId::resolved(&self.local_host_ref);
        for environment in &environments {
            let host_ref = &environment.spec.docker.as_ref().expect("Docker environment checked above").host_ref;
            let canonical = canonical_runtime_host_id(&self.daemon, &convoy.metadata.namespace, &local_host_id, host_ref).await?;
            if canonical != local_host_id {
                return Err(format!("Environment/{} is backed by remote host {host_ref}", environment.metadata.name));
            }
        }
        for environment in environments {
            let instance = environment.metadata.labels.get(flotilla_core::providers::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL);
            let (_, provider) = self
                .local_registry
                .environment_providers
                .select(flotilla_core::providers::environment::EnvironmentKind::Docker, instance.map(String::as_str))
                .ok_or_else(|| {
                    let reason = if instance.is_some() {
                        "instance absent or wrong kind"
                    } else if self
                        .local_registry
                        .environment_providers
                        .iter()
                        .any(|(_, provider)| provider.kind() == flotilla_core::providers::environment::EnvironmentKind::Docker)
                    {
                        "ambiguous instances"
                    } else {
                        "absent"
                    };
                    format!("Docker environment provider {reason} for Environment/{} (instance {instance:?})", environment.metadata.name)
                })?;
            let handles = provider
                .list()
                .await
                .map_err(|error| format!("Docker backing liveness check failed: {error}"))?
                .into_iter()
                .filter_map(|handle| Some(((handle.id().clone(), handle.container_name()?.to_string()), handle)))
                .collect::<HashMap<_, _>>();
            let Some(container_id) = environment.status.as_ref().and_then(|status| status.docker_container_id.as_ref()) else {
                return Err(format!(
                    "backing is not verifiable: Environment/{} has no Docker container identity",
                    environment.metadata.name
                ));
            };
            let key = (EnvironmentId::new(environment.metadata.name.clone()), container_id.clone());
            let Some(handle) = handles.get(&key) else {
                continue;
            };
            match handle.status().await {
                Ok(flotilla_protocol::EnvironmentStatus::Running) => {
                    return Err(format!("backing is live: Docker container {container_id} for Environment/{}", environment.metadata.name))
                }
                Ok(flotilla_protocol::EnvironmentStatus::Stopped | flotilla_protocol::EnvironmentStatus::Failed(_)) => {}
                Ok(status) => {
                    return Err(format!(
                        "backing is not verified dead: Docker container {container_id} for Environment/{} is {status:?}",
                        environment.metadata.name
                    ))
                }
                Err(error) => {
                    return Err(format!(
                        "backing is not verifiable: Docker container {container_id} for Environment/{}: {error}",
                        environment.metadata.name
                    ))
                }
            }
        }
        Ok(())
    }
}
