use std::{
    collections::{hash_map::Entry, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::HostDetector;
use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    EnvironmentId, EnvironmentInfo, EnvironmentStatus,
};
use flotilla_resources::host_direct_environment_name;

use async_trait::async_trait;
use flotilla_config::ConfigStore;
use flotilla_provider_runtime::host_environment::HostEnvironment;

use crate::environments::{CreateProvisionedEnvironmentRequest, EnvironmentLifecycle, Environments};
use crate::registry::ProviderRegistry;
use crate::run_host_detectors;
use crate::run_provisioned_host_detectors;
use crate::DiscoveryRuntime;
use crate::FactoryRegistry;
use flotilla_environment::environment::contained_daemon_socket_path;
use flotilla_environment::environment::CreateOpts;
use flotilla_environment::environment::EnvironmentHandle;
use flotilla_environment::environment::EnvironmentTool;
use flotilla_environment::environment::EnvironmentToolAsset;
use flotilla_environment::environment::EnvironmentToolAssetAccess;
use flotilla_environment::environment::EnvironmentToolAssetKind;
use flotilla_environment::environment::EnvironmentVariableUpdate;
use flotilla_environment::environment::ProvisionedMount;
use flotilla_environment::environment::ProvisionedMountMode;
use flotilla_environment::environment::CONTAINED_DAEMON_REQUIRED_ENV;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;

#[derive(Clone)]
pub enum ManagedEnvironmentKind {
    Direct(DirectEnvironmentState),
    Provisioned(ProvisionedEnvironmentState),
}

#[derive(Clone)]
pub struct DirectEnvironmentState {
    pub runner: Arc<dyn CommandRunner>,
    pub env_bag: EnvironmentBag,
    pub host_id: Option<HostId>,
    pub display_name: Option<String>,
    pub registry: Option<Arc<ProviderRegistry>>,
    pub ssh_destination: Option<String>,
    /// The host's declared supplies, which its SSH runner composes into every
    /// command and a session launch composes into the harness environment.
    pub host_environment: HostEnvironment,
    /// An agentless SSH host's declared vessel cap (#3793).
    pub max_vessels: Option<u32>,
}

#[derive(Clone)]
pub struct ProvisionedEnvironmentState {
    pub handle: EnvironmentHandle,
    pub env_bag: EnvironmentBag,
    pub display_name: Option<String>,
    pub registry: Option<Arc<ProviderRegistry>>,
    pub owning_host_id: HostId,
    pub provisioned_mounts: Vec<ProvisionedMount>,
}

/// The registered identity and runner selected for an environment resource reference.
pub struct ResolvedEnvironment {
    pub id: EnvironmentId,
    pub runner: Arc<dyn CommandRunner>,
}

pub struct EnvironmentManager {
    local_environment_id: EnvironmentId,
    local_host_id: HostId,
    managed: Mutex<HashMap<EnvironmentId, ManagedEnvironmentKind>>,
    host_detectors: Arc<Vec<Box<dyn HostDetector>>>,
}

impl EnvironmentManager {
    pub async fn new_local(discovery: &DiscoveryRuntime, local_environment_id: EnvironmentId, local_host_id: HostId) -> Self {
        let env_bag = run_host_detectors(&discovery.host_detectors, &*discovery.runner, &*discovery.env).await;
        Self::from_local_state(
            local_environment_id,
            local_host_id,
            Arc::clone(&discovery.runner),
            env_bag,
            Arc::clone(&discovery.host_detectors),
        )
    }

    /// Register an already-detected local environment and retain its detector
    /// configuration for subsequent provisioned-environment discovery.
    /// An empty collection disables detector probes for provisioned environments;
    /// their complete environment variables still populate the discovery bag.
    pub fn from_local_state(
        local_environment_id: EnvironmentId,
        local_host_id: HostId,
        local_runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
        host_detectors: Arc<Vec<Box<dyn HostDetector>>>,
    ) -> Self {
        let mut managed = HashMap::new();
        let display_name = Self::display_name_for_bag(&env_bag);
        managed.insert(
            local_environment_id.clone(),
            ManagedEnvironmentKind::Direct(DirectEnvironmentState {
                runner: Arc::clone(&local_runner),
                env_bag,
                host_id: Some(local_host_id.clone()),
                display_name,
                registry: None,
                ssh_destination: None,
                host_environment: HostEnvironment::default(),
                max_vessels: None,
            }),
        );

        Self { local_environment_id, local_host_id, managed: Mutex::new(managed), host_detectors }
    }

    pub fn local_environment_id(&self) -> &EnvironmentId {
        &self.local_environment_id
    }

    pub fn local_host_id(&self) -> &HostId {
        &self.local_host_id
    }

    pub fn local_environment_bag(&self) -> EnvironmentBag {
        self.environment_bag(&self.local_environment_id).expect("local direct environment must be registered in EnvironmentManager")
    }

    pub fn register_direct_environment(
        &self,
        env_id: EnvironmentId,
        runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
        host_id: Option<HostId>,
    ) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.entry(env_id.clone()) {
            Entry::Occupied(entry) => match entry.get() {
                ManagedEnvironmentKind::Direct(_) if env_id == self.local_environment_id => {
                    Err(format!("cannot replace local direct environment {env_id}"))
                }
                ManagedEnvironmentKind::Direct(_) => Err(format!("direct environment already registered: {env_id}")),
                ManagedEnvironmentKind::Provisioned(_) => {
                    Err(format!("cannot replace provisioned environment {env_id} with a direct environment"))
                }
            },
            Entry::Vacant(entry) => {
                let display_name = Self::display_name_for_bag(&env_bag);
                entry.insert(ManagedEnvironmentKind::Direct(DirectEnvironmentState {
                    runner,
                    env_bag,
                    host_id,
                    display_name,
                    registry: None,
                    ssh_destination: None,
                    host_environment: HostEnvironment::default(),
                    max_vessels: None,
                }));
                Ok(())
            }
        }
    }

    pub fn update_direct_environment_bag(&self, env_id: &EnvironmentId, env_bag: EnvironmentBag) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get_mut(env_id) {
            Some(ManagedEnvironmentKind::Direct(state)) => {
                state.display_name = Self::display_name_for_bag(&env_bag);
                state.env_bag = env_bag;
                Ok(())
            }
            Some(ManagedEnvironmentKind::Provisioned(_)) => Err(format!("environment is provisioned, not direct: {env_id}")),
            None => Err(format!("direct environment not found: {env_id}")),
        }
    }

    /// Resolve the resource reference written into checkout and session specs.
    /// The local host-direct resource names the existing local environment;
    /// it must not be registered as a second direct environment. Remote direct
    /// references and provisioned IDs resolve through their registered state.
    pub fn resolve_environment_ref(&self, env_ref: &str) -> Option<ResolvedEnvironment> {
        let env_id = if env_ref == host_direct_environment_name(self.local_host_id.as_str()) {
            self.local_environment_id.clone()
        } else {
            EnvironmentId::new(env_ref)
        };
        let runner = self.environment_runner(&env_id)?;
        Some(ResolvedEnvironment { id: env_id, runner })
    }

    pub fn environment_runner(&self, env_id: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(state) => Some(state.runner),
            ManagedEnvironmentKind::Provisioned(state) => Some(state.handle.runner()),
        }
    }

    pub fn environment_bag(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(state) => Some(state.env_bag),
            ManagedEnvironmentKind::Provisioned(state) => Some(state.env_bag),
        }
    }

    pub fn environment_registry(&self, env_id: &EnvironmentId) -> Option<Arc<ProviderRegistry>> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(state) => state.registry,
            ManagedEnvironmentKind::Provisioned(state) => state.registry,
        }
    }

    pub fn set_direct_environment_registry(&self, env_id: &EnvironmentId, registry: Arc<ProviderRegistry>) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get_mut(env_id) {
            Some(ManagedEnvironmentKind::Direct(state)) => {
                state.registry = Some(registry);
                Ok(())
            }
            Some(ManagedEnvironmentKind::Provisioned(_)) => Err(format!("environment is provisioned, not direct: {env_id}")),
            None => Err(format!("direct environment not found: {env_id}")),
        }
    }

    pub fn set_direct_environment_ssh_destination(&self, env_id: &EnvironmentId, destination: String) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get_mut(env_id) {
            Some(ManagedEnvironmentKind::Direct(state)) => {
                state.ssh_destination = Some(destination);
                Ok(())
            }
            _ => Err(format!("direct environment not found: {env_id}")),
        }
    }

    pub fn set_direct_environment_host_environment(&self, env_id: &EnvironmentId, host_environment: HostEnvironment) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get_mut(env_id) {
            Some(ManagedEnvironmentKind::Direct(state)) => {
                state.host_environment = host_environment;
                Ok(())
            }
            _ => Err(format!("direct environment not found: {env_id}")),
        }
    }

    pub fn set_direct_environment_max_vessels(&self, env_id: &EnvironmentId, max_vessels: Option<u32>) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get_mut(env_id) {
            Some(ManagedEnvironmentKind::Direct(state)) => {
                state.max_vessels = max_vessels;
                Ok(())
            }
            _ => Err(format!("direct environment not found: {env_id}")),
        }
    }

    pub fn environment_container_name(&self, env_id: &EnvironmentId) -> Option<String> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(_) => None,
            ManagedEnvironmentKind::Provisioned(state) => state.handle.container_name().map(ToString::to_string),
        }
    }

    pub fn provisioned_mounts(&self, env_id: &EnvironmentId) -> Option<Vec<ProvisionedMount>> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(_) => None,
            ManagedEnvironmentKind::Provisioned(state) => Some(state.provisioned_mounts.clone()),
        }
    }

    pub fn resolve_environment_path_to_host_path(&self, env_id: &EnvironmentId, path: &ExecutionEnvironmentPath) -> Option<QualifiedPath> {
        let state = match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(_) => return None,
            ManagedEnvironmentKind::Provisioned(state) => state,
        };
        if !Self::is_clean_absolute_path(path.as_path()) {
            return None;
        }
        let host_id = self.host_id_for_environment(env_id)?;
        let mount = Self::best_matching_mount(&state.provisioned_mounts, path.as_path(), MountDomain::Environment)?;
        let suffix = path.as_path().strip_prefix(mount.environment_path.as_path()).ok()?;
        Some(QualifiedPath::host(host_id, mount.host_path.as_path().join(suffix)))
    }

    pub fn resolve_host_path_to_environment_path(&self, env_id: &EnvironmentId, path: &QualifiedPath) -> Option<ExecutionEnvironmentPath> {
        let state = match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(_) => return None,
            ManagedEnvironmentKind::Provisioned(state) => state,
        };
        let host_id = self.host_id_for_environment(env_id)?;
        if !path.is_owned_by_host_id(&host_id) {
            return None;
        }
        if !Self::is_clean_absolute_path(path.path.as_path()) {
            return None;
        }
        let mount = Self::best_matching_mount(&state.provisioned_mounts, path.path.as_path(), MountDomain::Host)?;
        let suffix = path.path.strip_prefix(mount.host_path.as_path()).ok()?;
        Some(ExecutionEnvironmentPath::new(mount.environment_path.as_path().join(suffix)))
    }

    pub async fn host_summary_environments(&self) -> Vec<EnvironmentInfo> {
        let mut environments = Vec::new();
        for (env_id, state) in self.managed_environments() {
            if let ManagedEnvironmentKind::Provisioned(state) = state {
                let status = environment_display_status(state.handle.observe().await.state);
                environments.push(EnvironmentInfo::Provisioned {
                    id: env_id,
                    display_name: state.display_name.clone(),
                    image: state.handle.image().clone(),
                    status,
                });
            }
        }
        environments.sort_by(|a, b| Self::environment_info_sort_key(a).cmp(&Self::environment_info_sort_key(b)));
        environments
    }

    pub async fn visible_environments(&self) -> Vec<EnvironmentInfo> {
        let mut environments = Vec::new();
        for (env_id, state) in self.managed_environments() {
            match state {
                ManagedEnvironmentKind::Direct(state) => {
                    environments.push(EnvironmentInfo::Direct {
                        id: state.host_id.as_ref().map(|host_id| EnvironmentId::host(host_id.clone())).unwrap_or(env_id),
                        host_id: state.host_id.clone(),
                        display_name: state.display_name.clone(),
                        status: EnvironmentStatus::Running,
                    });
                }
                ManagedEnvironmentKind::Provisioned(state) => {
                    let status = environment_display_status(state.handle.observe().await.state);
                    environments.push(EnvironmentInfo::Provisioned {
                        id: env_id,
                        display_name: state.display_name.clone(),
                        image: state.handle.image().clone(),
                        status,
                    });
                }
            }
        }
        environments.sort_by(|a, b| Self::environment_info_sort_key(a).cmp(&Self::environment_info_sort_key(b)));
        environments
    }

    pub async fn create_provisioned_environment(&self, request: CreateProvisionedEnvironmentRequest<'_>) -> Result<(), String> {
        let CreateProvisionedEnvironmentRequest {
            env_id,
            provider,
            registry,
            prepared,
            tokens,
            config_base,
            daemon_socket_path,
            reference_repo,
        } = request;
        let (_, env_provider) =
            registry.environment_providers.select(provider).ok_or_else(|| format!("environment provider not available: {provider}"))?;

        let capabilities = env_provider.capabilities();
        let provisioned_mounts = reference_repo
            .as_ref()
            .zip(capabilities.reference_root.as_ref())
            .map(|(repo, root)| vec![ProvisionedMount::new(repo.as_path().to_path_buf(), root.clone(), ProvisionedMountMode::Ro)])
            .unwrap_or_default();
        let environment_socket_path = contained_daemon_socket_path(daemon_socket_path.as_path());
        let opts = CreateOpts {
            tokens,
            working_directory: None,
            provisioned_mounts,
            tools: vec![EnvironmentTool::new("flotilla-daemon-access", "/usr/local/bin/flotilla")
                .with_asset(EnvironmentToolAsset::new(
                    daemon_socket_path.as_path().to_path_buf(),
                    environment_socket_path.clone(),
                    EnvironmentToolAssetKind::UnixSocket,
                    EnvironmentToolAssetAccess::SharedWritable,
                    "the daemon socket",
                ))
                .with_environment(EnvironmentVariableUpdate::set(
                    "FLOTILLA_DAEMON_SOCKET",
                    environment_socket_path.to_string_lossy(),
                    "the daemon socket",
                ))
                .with_environment(EnvironmentVariableUpdate::set(
                    CONTAINED_DAEMON_REQUIRED_ENV,
                    "1",
                    "the contained host-daemon requirement",
                ))],
            image_pull_policy: Default::default(),
            prepared_auth: None,
            cpu_limit: None,
            memory_policy: Default::default(),
        };
        let handle = env_provider.provision(env_id.clone(), &prepared, opts.into()).await?;
        let (env_bag, provider_registry) = self.probe_provisioned_environment(&env_id, &handle, config_base).await?;
        self.register_provisioned_environment(env_id, handle, env_bag, Some(Arc::new(provider_registry)))
    }

    pub async fn ensure_provisioned_environment_providers(
        &self,
        env_id: &EnvironmentId,
        config_base: &DaemonHostPath,
    ) -> Result<(), String> {
        let state = self.provisioned_environment(env_id)?;
        if state.registry.is_some() {
            return Ok(());
        }

        let (bag, provider_registry) = self.probe_provisioned_environment(env_id, &state.handle, config_base).await?;
        self.update_provisioned_environment_discovery(env_id, &state.handle, bag, Some(Arc::new(provider_registry)));
        Ok(())
    }

    pub async fn destroy_provisioned_environment(&self, env_id: &EnvironmentId) -> Result<(), String> {
        let state = self.provisioned_environment(env_id)?;
        match state.handle.destroy().await {
            Ok(()) => {
                let _ = self.remove_provisioned_environment(env_id);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub fn register_provisioned_environment(
        &self,
        env_id: EnvironmentId,
        handle: EnvironmentHandle,
        env_bag: EnvironmentBag,
        registry: Option<Arc<ProviderRegistry>>,
    ) -> Result<(), String> {
        if handle.id() != &env_id {
            return Err(format!("provisioned environment id mismatch: key={env_id}, handle={}", handle.id()));
        }
        let provisioned_mounts = handle.provisioned_mounts();

        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.entry(env_id.clone()) {
            Entry::Occupied(mut entry) => {
                if matches!(entry.get(), ManagedEnvironmentKind::Direct(_)) {
                    return Err(format!("cannot replace direct environment {env_id} with a provisioned environment"));
                }
                entry.insert(ManagedEnvironmentKind::Provisioned(ProvisionedEnvironmentState {
                    handle,
                    display_name: Self::display_name_for_bag(&env_bag),
                    env_bag,
                    registry,
                    owning_host_id: self.local_host_id.clone(),
                    provisioned_mounts: provisioned_mounts.clone(),
                }));
            }
            Entry::Vacant(entry) => {
                entry.insert(ManagedEnvironmentKind::Provisioned(ProvisionedEnvironmentState {
                    handle,
                    display_name: Self::display_name_for_bag(&env_bag),
                    env_bag,
                    registry,
                    owning_host_id: self.local_host_id.clone(),
                    provisioned_mounts,
                }));
            }
        }
        Ok(())
    }

    pub fn remove_provisioned_environment(&self, env_id: &EnvironmentId) -> Option<ProvisionedEnvironmentState> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get(env_id) {
            Some(ManagedEnvironmentKind::Direct(_)) => None,
            Some(ManagedEnvironmentKind::Provisioned(_)) => match managed.remove(env_id) {
                Some(ManagedEnvironmentKind::Provisioned(state)) => Some(state),
                _ => None,
            },
            None => None,
        }
    }

    fn managed_environment(&self, env_id: &EnvironmentId) -> Option<ManagedEnvironmentKind> {
        self.managed.lock().expect("environment manager lock poisoned").get(env_id).cloned()
    }

    pub fn managed_environments(&self) -> Vec<(EnvironmentId, ManagedEnvironmentKind)> {
        let mut managed: Vec<_> = self
            .managed
            .lock()
            .expect("environment manager lock poisoned")
            .iter()
            .map(|(env_id, state)| (env_id.clone(), state.clone()))
            .collect();
        managed.sort_by(|a, b| a.0.cmp(&b.0));
        managed
    }

    pub fn replace_local_environment_bag(&self, env_bag: EnvironmentBag) -> Result<(), String> {
        self.update_direct_environment_bag(&self.local_environment_id, env_bag)
    }

    fn update_provisioned_environment_discovery(
        &self,
        env_id: &EnvironmentId,
        expected_handle: &EnvironmentHandle,
        env_bag: EnvironmentBag,
        registry: Option<Arc<ProviderRegistry>>,
    ) {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        let Some(ManagedEnvironmentKind::Provisioned(state)) = managed.get_mut(env_id) else {
            return;
        };

        if Arc::ptr_eq(&state.handle, expected_handle) {
            state.display_name = Self::display_name_for_bag(&env_bag);
            state.env_bag = env_bag;
            state.registry = registry;
        }
    }

    fn provisioned_environment(&self, env_id: &EnvironmentId) -> Result<ProvisionedEnvironmentState, String> {
        match self.managed_environment(env_id) {
            Some(ManagedEnvironmentKind::Provisioned(state)) => Ok(state),
            Some(ManagedEnvironmentKind::Direct(_)) | None => Err(format!("environment handle not found: {env_id}")),
        }
    }

    async fn probe_provisioned_environment(
        &self,
        env_id: &EnvironmentId,
        handle: &EnvironmentHandle,
        config_base: &DaemonHostPath,
    ) -> Result<(EnvironmentBag, ProviderRegistry), String> {
        let env_runner = handle.runner();

        let raw_env_vars = handle.env_vars().await?;
        let bag = run_provisioned_host_detectors(&self.host_detectors, &*env_runner, &raw_env_vars).await;

        let config = ConfigStore::with_base(config_base.as_path().join(format!("env-discovery/{env_id}")));
        let env_repo_root = ExecutionEnvironmentPath::new(handle.capabilities().workspace_root);
        // Environment lifetimes own host capabilities. Checkout VCS and forge
        // capabilities are discovered by their resource-scoped consumers.
        let mut factories = FactoryRegistry::default_all();
        factories.vcs.clear();
        factories.change_requests.clear();
        factories.issue_trackers.clear();
        let provider_registry = factories.probe_all(&bag, &config, &env_repo_root, env_runner).await;

        Ok((bag, provider_registry))
    }

    fn display_name_for_bag(env_bag: &EnvironmentBag) -> Option<String> {
        env_bag.find_env_var("DISPLAY_NAME").map(|value| value.to_owned())
    }

    fn is_clean_absolute_path(path: &Path) -> bool {
        use std::path::Component;

        let raw = path.as_os_str().to_string_lossy();
        path.is_absolute()
            && raw
                .strip_prefix('/')
                .is_some_and(|suffix| suffix.split('/').all(|component| !component.is_empty() && component != "." && component != ".."))
            && path.components().all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
    }

    pub fn host_id_for_environment(&self, env_id: &EnvironmentId) -> Option<HostId> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(state) => state.host_id.clone(),
            ManagedEnvironmentKind::Provisioned(state) => Some(state.owning_host_id.clone()),
        }
    }

    fn best_matching_mount<'a>(mounts: &'a [ProvisionedMount], path: &Path, domain: MountDomain) -> Option<&'a ProvisionedMount> {
        mounts
            .iter()
            .filter(|mount| match domain {
                MountDomain::Environment => path.starts_with(mount.environment_path.as_path()),
                MountDomain::Host => path.starts_with(mount.host_path.as_path()),
            })
            .max_by_key(|mount| match domain {
                MountDomain::Environment => mount.environment_path.as_path().components().count(),
                MountDomain::Host => mount.host_path.as_path().components().count(),
            })
    }

    fn environment_info_sort_key(info: &EnvironmentInfo) -> (&EnvironmentId, u8) {
        match info {
            EnvironmentInfo::Direct { id, .. } => (id, 0),
            EnvironmentInfo::Provisioned { id, .. } => (id, 1),
        }
    }
}

impl Environments for EnvironmentManager {
    fn local_host_id(&self) -> &HostId {
        EnvironmentManager::local_host_id(self)
    }
    fn environment_bag(&self, environment: &EnvironmentId) -> Option<EnvironmentBag> {
        EnvironmentManager::environment_bag(self, environment)
    }
    fn environment_runner(&self, environment: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        EnvironmentManager::environment_runner(self, environment)
    }
    fn environment_registry(&self, environment: &EnvironmentId) -> Option<Arc<ProviderRegistry>> {
        EnvironmentManager::environment_registry(self, environment)
    }
    fn host_id_for_environment(&self, environment: &EnvironmentId) -> Option<HostId> {
        EnvironmentManager::host_id_for_environment(self, environment)
    }
}

#[async_trait]
impl EnvironmentLifecycle for EnvironmentManager {
    async fn ensure_provisioned_environment_providers(
        &self,
        environment: &EnvironmentId,
        config_base: &DaemonHostPath,
    ) -> Result<(), String> {
        EnvironmentManager::ensure_provisioned_environment_providers(self, environment, config_base).await
    }
    async fn create_provisioned_environment(&self, request: CreateProvisionedEnvironmentRequest<'_>) -> Result<(), String> {
        EnvironmentManager::create_provisioned_environment(self, request).await
    }
    async fn destroy_provisioned_environment(&self, environment: &EnvironmentId) -> Result<(), String> {
        EnvironmentManager::destroy_provisioned_environment(self, environment).await
    }
}

#[derive(Clone, Copy)]
enum MountDomain {
    Host,
    Environment,
}

// The legacy query surface has fewer states; operational consumers use observe.
fn environment_display_status(state: flotilla_environment::environment::ObservationState) -> EnvironmentStatus {
    use flotilla_environment::environment::ObservationState;
    match state {
        ObservationState::Running => EnvironmentStatus::Running,
        ObservationState::Stopped { .. } | ObservationState::Gone { .. } => EnvironmentStatus::Stopped,
        ObservationState::Unknown { reason: flotilla_environment::environment::UnknownReason::GuestUnresponsive } => {
            EnvironmentStatus::Starting
        }
        ObservationState::Unknown { reason } => EnvironmentStatus::Failed(format!("{reason:?}")),
    }
}
