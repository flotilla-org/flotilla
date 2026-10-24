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
use flotilla_admission::convoy_ensure::StandingConvoyBackingInspector;
use flotilla_agent::process::ExitReceiptObserver;
use flotilla_config::ConfigStore;
use flotilla_config::DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_credentials::{AgentMaterialRegistry, CredentialStore};
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::{CanonicalHostId, EnvironmentId};
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use flotilla_provisioning::reconcilers::clone::runtime::CloneFlights;
use flotilla_provisioning::reconcilers::{CheckoutRemoval, ForgeDefaultBranchResolver};
use flotilla_resources::{Convoy, ConvoyProvisioningState, Environment, Host, HostSpec, ResourceError, ResourceObject};
use flotilla_store::load_metrics::ControllerMetricsRegistry;
use tokio::sync::{Mutex, Semaphore};

use super::discovery::AgentlessSshProfile;
use super::environments::{stage_remote_rustc_wrapper, ActiveProvisionedEnvironment};
use super::tasks::{load_checkout_archive_roots, CheckoutArchiveRoot};
use super::terminal::PendingTerminalDelivery;
#[cfg(test)]
use crate::server::environment_sockets::EnvironmentSocketRegistry;
use crate::server::{environment_sockets::EnvironmentSocketService, HookEndpoint};
use crate::{
    blob_store::TieredBlobStore,
    environment_tools::{stage_local_rustc_wrapper_async, EnvironmentToolProvisioner},
};

pub(super) struct ControllerRuntimeState {
    pub(super) daemon: Arc<InProcessDaemon>,
    pub(super) config: Arc<ConfigStore>,
    pub(super) local_registry: Arc<ProviderRegistry>,
    pub(super) environment_providers: flotilla_discovery::registry::EnvironmentProviderRegistry,
    pub(super) local_host_ref: String,
    /// OS probe boundary, injectable for deterministic launch tests.
    pub(super) local_core_count: fn() -> Result<NonZeroUsize, std::io::Error>,
    pub(super) namespace: String,
    pub(super) host_direct_environment_name: String,
    pub(super) agentless_ssh: HashMap<String, AgentlessSshProfile>,
    pub(super) environment_tools: EnvironmentToolProvisioner,
    pub(super) environment_sockets: Option<EnvironmentSocketService>,
    /// The vessel agents on agentless hosts (#3792).
    pub(super) agentless_vessels: Option<Arc<super::agentless_vessels::AgentlessVessels>>,
    /// Where host-direct Claude Code crews post their hooks, when flotillad listens on TCP.
    pub(super) hook_endpoint: Option<HookEndpoint>,
    pub(super) credential_store: Option<Arc<CredentialStore>>,
    pub(super) agent_material: Option<Arc<AgentMaterialRegistry>>,
    pub(super) blob_store: Option<Arc<TieredBlobStore>>,
    pub(super) image_inventory: Option<Arc<flotilla_provisioning::image_inventory::ImageInventoryPublisher<super::ports::ProviderImageIo>>>,
    /// This host's ADR 0062 image build service, when it can build images.
    pub(super) image_build_service: Option<Arc<dyn flotilla_environment::environment::ImageBuildService>>,
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
    /// Forge default-branch observations consulted before the GitHub CLI.
    pub(super) forge_default_branches: Option<Arc<dyn ForgeDefaultBranchResolver>>,
    /// Per-kind controller loop load metrics (#3206).
    pub(super) controller_metrics: Arc<ControllerMetricsRegistry>,
    /// The provisioning kinds' engine homes behind `[primitives]` (#3629): the single writer
    /// of the resources they hold. Absent with the switch off.
    pub(super) provisioning: Option<flotilla_provisioning::engine::ProvisioningLifecycles>,
}

impl ControllerRuntimeState {
    pub(super) async fn provider_address(
        &self,
        name: &str,
        spec: &flotilla_resources::EnvironmentSpec,
        instance: Option<&str>,
    ) -> Result<flotilla_address::Address, String> {
        use flotilla_environment::environment::EnvironmentInput;
        let mut address = if let Some(address) = &spec.provider_address {
            flotilla_resources::validate_environment_provider_address(address)?;
            address.clone()
        } else {
            let host = spec.host_ref().ok_or_else(|| format!("environment {name} has no provider address or legacy host"))?;
            let local = CanonicalHostId::resolved(&self.local_host_ref);
            let host = canonical_runtime_host_id(&self.daemon, &self.namespace, &local, host).await?;
            self.environment_providers.resolve_legacy(host.as_str(), EnvironmentInput::of(spec)?, instance)?
        };
        // A legacy host_ref may be a display alias. Metadata-aware decoding can
        // already qualify its instance; retain the same Host authority lookup
        // for the place without ever substituting another provider instance.
        if self.environment_providers.get(&address).is_err() {
            if let Some(host) = address.get(flotilla_address::Kind::Host) {
                let local = CanonicalHostId::resolved(&self.local_host_ref);
                let canonical = canonical_runtime_host_id(&self.daemon, &self.namespace, &local, host).await?;
                address = flotilla_address::Address::root(flotilla_address::Kind::Host, canonical.as_str())
                    .and_then(|place| place.child(flotilla_address::Kind::Provider, address.name()))
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(address)
    }

    pub(super) fn with_environment_sockets(mut self, sockets: Option<EnvironmentSocketService>) -> Self {
        self.environment_sockets = sockets;
        self
    }

    /// Agentless crews reach the daemon only through their vessel agent's
    /// forward to an environment socket, so vessels exist exactly when the
    /// socket server does.
    pub(super) fn with_agentless_vessels(mut self, vessels: Option<Arc<super::agentless_vessels::AgentlessVessels>>) -> Self {
        self.agentless_vessels = vessels;
        self
    }

    pub(super) fn with_hook_endpoint(mut self, endpoint: Option<HookEndpoint>) -> Self {
        self.hook_endpoint = endpoint;
        self
    }

    pub(super) async fn rust_build_jobs(&self, host_ref: &str) -> Result<usize, String> {
        self.rust_build_jobs_through(host_ref, None).await
    }

    /// As [`Self::rust_build_jobs`], counting an agentless host's cores through
    /// `runner` (a vessel's agent, #3792) rather than its SSH runner.
    pub(super) async fn rust_build_jobs_through(&self, host_ref: &str, runner: Option<&dyn CommandRunner>) -> Result<usize, String> {
        let host_spec = match self.daemon.resource_backend().using::<Host>(&self.namespace).get(host_ref).await {
            Ok(host) => host.spec,
            Err(ResourceError::NotFound { .. }) if host_ref == self.local_host_ref => HostSpec::default(),
            Err(error) => return Err(format!("read build fulfilment setting for host {host_ref}: {error}")),
        };
        let cores = if let Some(profile) = self.agentless_ssh_for_host(host_ref) {
            runner
                .unwrap_or(profile.runner.as_ref())
                .run("getconf", &["_NPROCESSORS_ONLN"], Path::new("/"), &ChannelLabel::Default)
                .await?
                .trim()
                .parse::<usize>()
                .map_err(|error| format!("read core count for host {host_ref}: {error}"))?
        } else if host_ref == self.local_host_ref {
            (self.local_core_count)().map(usize::from).map_err(|error| format!("read local core count: {error}"))?
        } else {
            return Err(format!("no core count source for host {host_ref}"));
        };
        Ok(host_spec.rust_build_jobs(cores))
    }

    /// The prepared vessel on an agentless host (#3792).
    pub(super) async fn vessel_access(
        &self,
        profile: &AgentlessSshProfile,
        vessel: &str,
    ) -> Result<Arc<super::agentless_vessels::VesselAccess>, String> {
        let vessels =
            self.agentless_vessels.as_ref().ok_or("EnvironmentSocketUnavailable: agentless vessels require a crew socket server")?;
        let preparation = super::agentless_vessels::VesselPreparation {
            tools: &self.environment_tools,
            discovery: self.daemon.discovery_runtime(),
            config: &self.config,
        };
        vessels.access(&preparation, profile, vessel).await
    }

    /// The Rust linker wrapper for a session in `env_ref`, staged on an
    /// agentless host through `runner` when given (a vessel's agent, #3792).
    pub(super) async fn rustc_wrapper_for_environment(&self, env_ref: &str, runner: Option<&dyn CommandRunner>) -> Result<PathBuf, String> {
        if let Some(profile) = self.agentless_ssh.get(env_ref) {
            let runner = runner.unwrap_or(profile.runner.as_ref());
            let base = runner.writable_config_base(None, self.config.state_dir().as_path()).await?;
            return stage_remote_rustc_wrapper(runner, &base).await;
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
        let mut environment_providers = flotilla_discovery::registry::EnvironmentProviderRegistry::default();
        let place = flotilla_address::Address::root(flotilla_address::Kind::Host, &local_host_ref).expect("local host identity");
        environment_providers.register_place(&place, &local_registry.environment_providers).expect("unique local provider addresses");
        let account_classes = [flotilla_protocol::provider::EnvironmentClass::CloudSandbox];
        for (_, provider) in local_registry.environment_providers.iter().filter(|(_, p)| account_classes.contains(&p.kind().class)) {
            let instance = local_registry.environment_providers.instance_name(provider).expect("registered instance");
            let address = flotilla_address::Address::root(flotilla_address::Kind::Account, instance)
                .and_then(|p| p.child(flotilla_address::Kind::Provider, instance))
                .expect("account provider address");
            environment_providers.insert(address, provider.clone()).expect("unique cloud provider address");
        }
        Self {
            environment_providers,
            local_core_count: std::thread::available_parallelism,
            daemon,
            config,
            local_registry,
            local_host_ref,
            namespace: flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE.to_string(),
            host_direct_environment_name,
            agentless_ssh: HashMap::new(),
            environment_tools,
            environment_sockets: None,
            agentless_vessels: None,
            hook_endpoint: None,
            credential_store: None,
            agent_material: None,
            blob_store: None,
            image_inventory: None,
            image_build_service: None,
            provisioned_environments: Mutex::new(HashMap::new()),
            local_backing_observed: AtomicBool::new(false),
            clone_flights: Arc::new(CloneFlights::default()),
            terminal_deliveries: StdMutex::new(HashMap::new()),
            exit_receipts: ExitReceiptObserver::default(),
            archive_catalog_lock: Mutex::new(()),
            checkout_removal_concurrency: DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY,
            checkout_removals: Semaphore::new(DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY.get()),
            forge_default_branches: None,
            controller_metrics: Arc::default(),
            provisioning: None,
        }
    }

    pub(super) fn with_provisioning_lifecycles(
        mut self,
        lifecycles: Option<flotilla_provisioning::engine::ProvisioningLifecycles>,
    ) -> Self {
        self.provisioning = lifecycles;
        self
    }

    pub(super) fn with_controller_metrics(mut self, metrics: Arc<ControllerMetricsRegistry>) -> Self {
        self.controller_metrics = metrics;
        self
    }

    pub(super) fn with_forge_default_branches(mut self, resolver: Option<Arc<dyn ForgeDefaultBranchResolver>>) -> Self {
        self.forge_default_branches = resolver;
        self
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
        for profile in &profiles {
            if let Some(registry) = self.daemon.environment_registry_for_environment(&profile.environment_id) {
                let place = flotilla_address::Address::root(flotilla_address::Kind::Host, &profile.provisioning.host_id)
                    .expect("SSH host identity");
                self.environment_providers.register_place(&place, &registry.environment_providers).expect("unique SSH provider addresses");
            }
        }
        self.agentless_ssh = profiles.into_iter().map(|profile| (profile.environment_id.to_string(), profile)).collect();
        self
    }

    pub(super) fn with_namespace(mut self, namespace: String) -> Self {
        self.namespace = namespace;
        self
    }

    /// Profiles are keyed by their environment ID; this finds one by the Host
    /// it drives.
    pub(super) fn agentless_ssh_for_host(&self, host_ref: &str) -> Option<&AgentlessSshProfile> {
        self.agentless_ssh.values().find(|profile| profile.provisioning.host_id == host_ref)
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

    pub(super) fn with_image_inventory(
        mut self,
        publisher: Option<Arc<flotilla_provisioning::image_inventory::ImageInventoryPublisher<super::ports::ProviderImageIo>>>,
    ) -> Self {
        self.image_inventory = publisher;
        self
    }

    pub(super) fn with_image_build_service(
        mut self,
        service: Option<Arc<dyn flotilla_environment::environment::ImageBuildService>>,
    ) -> Self {
        self.image_build_service = service;
        self
    }

    pub(super) fn with_blob_store(mut self, blob_store: Arc<TieredBlobStore>) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    #[cfg(test)]
    pub(super) fn with_environment_tools(mut self, environment_tools: EnvironmentToolProvisioner) -> Self {
        // Test adapter for the socket-server process seam. Provisioning still
        // binds and retires real sockets; no Docker or live host is involved.
        self.environment_sockets = Some(EnvironmentSocketService::new(
            Arc::new(Mutex::new(EnvironmentSocketRegistry::new())),
            self.config.state_dir().join("env"),
            |listener, _, _| {
                tokio::spawn(async move {
                    let _listener = listener;
                    std::future::pending::<()>().await
                })
            },
        ));
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
            // Unlike the general provisioning profile's per_vessel capability
            // test, this recovery path reads legacy Docker status. Retire its
            // input filter with the legacy-input bridge at the table move (#3108).
            let providers = self
                .local_registry
                .environment_providers
                .iter()
                .filter(|(_, provider)| provider.legacy_input() == Some(flotilla_environment::environment::EnvironmentInput::Docker))
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
        // This standing-convoy expunge proof still consumes the preceding-
        // generation Docker status/host contract. Account and microVM recovery
        // is deliberately unproven here until its provider observation path is
        // wired in; refusal is never evidence that such a backing is dead.
        // The EnvironmentRuntime provisioning/teardown port has no such filter.
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
            let instance = environment.metadata.labels.get(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL);
            let address = self.provider_address(&environment.metadata.name, &environment.spec, instance.map(String::as_str)).await?;
            let provider = self.environment_providers.get(&address)?;
            let handles = provider
                .list()
                .await
                .map_err(|error| format!("Docker backing liveness check failed: {error}"))?
                .into_iter()
                .map(|handle| ((handle.id().clone(), handle.backing_id().to_string()), handle))
                .collect::<HashMap<_, _>>();
            let Some(container_id) = environment.status.as_ref().and_then(|status| status.docker_container_id.as_ref()) else {
                return Err(format!(
                    "backing is not verifiable: Environment/{} has no Docker container identity",
                    environment.metadata.name
                ));
            };
            let key = (EnvironmentId::new(environment.metadata.name.clone()), container_id.clone());
            // Old Docker status values were transport names, before opaque IDs.
            let Some(handle) = handles.get(&key).or_else(|| {
                handles.values().find(|handle| handle.id() == &key.0 && handle.container_name() == Some(container_id.as_str()))
            }) else {
                continue;
            };
            match handle.observe().await.state {
                flotilla_environment::environment::ObservationState::Running => {
                    return Err(format!("backing is live: Docker container {container_id} for Environment/{}", environment.metadata.name))
                }
                flotilla_environment::environment::ObservationState::Stopped { .. }
                | flotilla_environment::environment::ObservationState::Gone { .. } => {}
                flotilla_environment::environment::ObservationState::Unknown { reason } => {
                    return Err(format!(
                        "backing is not verifiable: Docker container {container_id} for Environment/{}: {reason:?}",
                        environment.metadata.name
                    ));
                }
            }
        }
        Ok(())
    }
}
