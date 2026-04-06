use std::{
    collections::{hash_map::Entry, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    EnvironmentId, EnvironmentInfo, EnvironmentStatus, ImageId,
};

use crate::{
    config::ConfigStore,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        discovery::{run_host_detectors, DiscoveryRuntime, EnvironmentAssertion, EnvironmentBag, FactoryRegistry},
        environment::{CreateOpts, EnvironmentHandle, ProvisionedMount},
        registry::ProviderRegistry,
        CommandRunner,
    },
};

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

pub struct EnvironmentManager {
    local_environment_id: EnvironmentId,
    local_host_id: HostId,
    managed: Mutex<HashMap<EnvironmentId, ManagedEnvironmentKind>>,
}

pub struct CreateProvisionedEnvironmentRequest<'a> {
    pub env_id: EnvironmentId,
    pub provider: &'a str,
    pub registry: &'a ProviderRegistry,
    pub image: ImageId,
    pub tokens: Vec<(String, String)>,
    pub config_base: &'a DaemonHostPath,
    pub daemon_socket_path: &'a DaemonHostPath,
    pub reference_repo: Option<DaemonHostPath>,
}

impl EnvironmentManager {
    pub async fn new_local(discovery: &DiscoveryRuntime, local_environment_id: EnvironmentId, local_host_id: HostId) -> Self {
        let env_bag = run_host_detectors(&discovery.host_detectors, &*discovery.runner, &*discovery.env).await;
        Self::from_local_state(local_environment_id, local_host_id, Arc::clone(&discovery.runner), env_bag)
    }

    pub fn from_local_state(
        local_environment_id: EnvironmentId,
        local_host_id: HostId,
        local_runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
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
            }),
        );

        Self { local_environment_id, local_host_id, managed: Mutex::new(managed) }
    }

    pub fn local_environment_id(&self) -> &EnvironmentId {
        &self.local_environment_id
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
                entry.insert(ManagedEnvironmentKind::Direct(DirectEnvironmentState { runner, env_bag, host_id, display_name }));
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
            ManagedEnvironmentKind::Direct(_) => None,
            ManagedEnvironmentKind::Provisioned(state) => state.registry,
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
                let status = state.handle.status().await.unwrap_or_else(EnvironmentStatus::Failed);
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
                        id: env_id,
                        host_id: state.host_id.clone(),
                        display_name: state.display_name.clone(),
                        status: EnvironmentStatus::Running,
                    });
                }
                ManagedEnvironmentKind::Provisioned(state) => {
                    let status = state.handle.status().await.unwrap_or_else(EnvironmentStatus::Failed);
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
            image,
            tokens,
            config_base,
            daemon_socket_path,
            reference_repo,
        } = request;
        let (_, env_provider) =
            registry.environment_providers.get(provider).ok_or_else(|| format!("environment provider not available: {provider}"))?;

        let provisioned_mounts = reference_repo
            .as_ref()
            .map(|repo| vec![ProvisionedMount::new(repo.as_path().to_path_buf(), PathBuf::from("/ref/repo"))])
            .unwrap_or_default();
        let opts = CreateOpts { tokens, daemon_socket_path: daemon_socket_path.clone(), working_directory: None, provisioned_mounts };
        let handle = env_provider.create(env_id.clone(), &image, opts).await?;
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_local_environment_bag_for_test(&self, env_bag: EnvironmentBag) -> Result<(), String> {
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
        let mut bag = EnvironmentBag::new();
        for (key, value) in &raw_env_vars {
            bag = bag.with(EnvironmentAssertion::env_var(key, value));
        }

        let config = ConfigStore::with_base(config_base.as_path().join(format!("env-discovery/{env_id}")));
        let env_repo_root = ExecutionEnvironmentPath::new("/workspace");
        let provider_registry = FactoryRegistry::default_all().probe_all(&bag, &config, &env_repo_root, env_runner).await;

        Ok((bag, provider_registry))
    }

    fn display_name_for_bag(env_bag: &EnvironmentBag) -> Option<String> {
        env_bag.find_env_var("DISPLAY_NAME").map(|value| value.to_owned())
    }

    fn is_clean_absolute_path(path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        path_str.starts_with('/') && path_str.split('/').all(|component| component != "." && component != "..")
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

#[derive(Clone, Copy)]
enum MountDomain {
    Host,
    Environment,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    use async_trait::async_trait;
    use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId};

    use super::*;
    use crate::providers::{
        discovery::{
            test_support::{fake_discovery, DiscoveryMockRunner},
            EnvironmentAssertion,
        },
        environment::{CreateOpts, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment, ProvisionedMount},
        registry::ProviderRegistry,
        CommandRunner,
    };

    struct MockProvisionedEnvironment {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
        env_vars: HashMap<String, String>,
        provisioned_mounts: Vec<ProvisionedMount>,
        destroyed: Arc<AtomicBool>,
        destroy_error: Option<String>,
    }

    #[async_trait]
    impl ProvisionedEnvironment for MockProvisionedEnvironment {
        fn id(&self) -> &EnvironmentId {
            &self.id
        }

        fn image(&self) -> &ImageId {
            &self.image
        }

        fn container_name(&self) -> Option<&str> {
            Some("mock-container")
        }

        async fn status(&self) -> Result<EnvironmentStatus, String> {
            Ok(EnvironmentStatus::Running)
        }

        async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
            Ok(self.env_vars.clone())
        }

        fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
            self.provisioned_mounts.clone()
        }

        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::clone(&self.runner)
        }

        async fn destroy(&self) -> Result<(), String> {
            self.destroyed.store(true, Ordering::SeqCst);
            match &self.destroy_error {
                Some(err) => Err(err.clone()),
                None => Ok(()),
            }
        }
    }

    struct MockEnvironmentProvider {
        create_result: tokio::sync::Mutex<Option<Result<EnvironmentHandle, String>>>,
    }

    #[async_trait]
    impl EnvironmentProvider for MockEnvironmentProvider {
        async fn ensure_image(&self, _spec: &EnvironmentSpec, _repo_root: &std::path::Path) -> Result<ImageId, String> {
            Err("unused in test".to_string())
        }

        async fn create(&self, _id: EnvironmentId, _image: &ImageId, _opts: CreateOpts) -> Result<EnvironmentHandle, String> {
            self.create_result.lock().await.take().expect("create called more than expected")
        }

        async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
            Ok(vec![])
        }
    }

    fn mock_handle(
        id: &EnvironmentId,
        env_vars: HashMap<String, String>,
        destroy_error: Option<String>,
    ) -> (EnvironmentHandle, Arc<AtomicBool>) {
        let destroyed = Arc::new(AtomicBool::new(false));
        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: id.clone(),
            image: ImageId::new("mock:image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars,
            provisioned_mounts: vec![],
            destroyed: Arc::clone(&destroyed),
            destroy_error,
        });
        (handle, destroyed)
    }

    fn test_local_environment_id() -> EnvironmentId {
        EnvironmentId::new("test-local-environment")
    }

    fn test_local_host_id() -> HostId {
        HostId::new("test-local-host-id")
    }

    fn reference_repo_mount() -> ProvisionedMount {
        ProvisionedMount::new("/host/reference-repo", "/ref/repo")
    }

    #[tokio::test]
    async fn new_local_registers_direct_environment() {
        let env_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, env_id.clone(), test_local_host_id()).await;

        assert_eq!(manager.local_environment_id(), &env_id);
        assert!(manager.environment_runner(&env_id).is_some());
        assert!(manager.environment_bag(&env_id).is_some());
        assert!(manager.environment_runner(&EnvironmentId::new("missing")).is_none());
    }

    #[tokio::test]
    async fn direct_environment_lookup_uses_host_detectors() {
        let env_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, env_id.clone(), test_local_host_id()).await;

        let bag = manager.environment_bag(&env_id).expect("local environment bag");
        assert!(bag.find_binary("git").is_some(), "host detectors should populate the direct environment bag");
    }

    #[tokio::test]
    async fn register_and_remove_provisioned_environment_updates_state() {
        let env_id = EnvironmentId::new("env-provisioned-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        assert!(
            manager.provisioned_mounts(&test_local_environment_id()).is_none(),
            "direct environments must not expose provisioned mounts"
        );
        let runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;
        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: env_id.clone(),
            image: ImageId::new("mock:image"),
            runner,
            env_vars: HashMap::new(),
            provisioned_mounts: vec![reference_repo_mount()],
            destroyed: Arc::new(AtomicBool::new(false)),
            destroy_error: None,
        });
        let env_bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("PROVISIONED", "true"));
        let registry = Arc::new(ProviderRegistry::new());

        manager
            .register_provisioned_environment(env_id.clone(), handle, env_bag.clone(), Some(Arc::clone(&registry)))
            .expect("register provisioned environment");

        let bag = manager.environment_bag(&env_id).expect("provisioned environment bag");
        assert_eq!(bag.find_env_var("PROVISIONED"), Some("true"));
        assert_eq!(manager.provisioned_mounts(&env_id).expect("provisioned mounts"), vec![reference_repo_mount()]);

        let lookup_registry = manager.environment_registry(&env_id).expect("provisioned environment registry");
        assert!(Arc::ptr_eq(&lookup_registry, &registry));

        let removed = manager.remove_provisioned_environment(&env_id).expect("provisioned environment removed");
        assert_eq!(removed.env_bag.find_env_var("PROVISIONED"), Some("true"));
        assert_eq!(removed.provisioned_mounts, vec![reference_repo_mount()]);
        assert!(manager.environment_bag(&env_id).is_none());
        assert!(manager.environment_registry(&env_id).is_none());
    }

    #[tokio::test]
    async fn register_provisioned_environment_rejects_handle_id_mismatch() {
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let registration_id = EnvironmentId::new("provisioned-registration-id");
        let handle_id = EnvironmentId::new("provisioned-handle-id");
        let (handle, _) = mock_handle(&handle_id, HashMap::new(), None);

        let result = manager.register_provisioned_environment(registration_id.clone(), handle, EnvironmentBag::new(), None);

        assert_eq!(result.unwrap_err(), format!("provisioned environment id mismatch: key={registration_id}, handle={handle_id}"));
        assert!(manager.environment_bag(&registration_id).is_none());
        assert!(manager.environment_bag(&handle_id).is_none());
    }

    #[tokio::test]
    async fn register_direct_environment_adds_an_independent_direct_environment() {
        let discovery = fake_discovery(false);
        let local_environment_id = test_local_environment_id();
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone(), test_local_host_id()).await;
        let direct_environment_id = EnvironmentId::new("direct-environment");
        let direct_runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;
        let direct_bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("DIRECT", "true"));

        manager
            .register_direct_environment(direct_environment_id.clone(), Arc::clone(&direct_runner), direct_bag.clone(), None)
            .expect("register direct environment");

        let managed_ids: Vec<_> = manager.managed_environments().into_iter().map(|(id, _)| id).collect();
        assert!(managed_ids.contains(&local_environment_id), "local direct environment should be enumerated");
        assert!(managed_ids.contains(&direct_environment_id), "new direct environment should be enumerated");
        assert!(Arc::ptr_eq(&manager.environment_runner(&direct_environment_id).expect("direct runner"), &direct_runner));
        assert_eq!(manager.environment_bag(&direct_environment_id).expect("direct bag").find_env_var("DIRECT"), Some("true"));
        assert!(manager.environment_runner(&local_environment_id).is_some(), "local direct environment should remain registered");
    }

    #[tokio::test]
    async fn visible_environments_includes_direct_and_provisioned_environments_with_display_names() {
        let local_environment_id = EnvironmentId::new("local-env");
        let local_bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("DISPLAY_NAME", "local-dev"));
        let manager = EnvironmentManager::from_local_state(
            local_environment_id.clone(),
            HostId::new("local-host-id"),
            Arc::new(DiscoveryMockRunner::builder().build()),
            local_bag,
        );

        let ssh_environment_id = EnvironmentId::new("ssh-env");
        manager
            .register_direct_environment(
                ssh_environment_id.clone(),
                Arc::new(DiscoveryMockRunner::builder().build()),
                EnvironmentBag::new().with(EnvironmentAssertion::env_var("DISPLAY_NAME", "ssh-dev")),
                Some(HostId::new("ssh-host-id")),
            )
            .expect("register ssh direct environment");

        let provisioned_environment_id = EnvironmentId::new("provisioned-env");
        let (handle, _) =
            mock_handle(&provisioned_environment_id, HashMap::from([(String::from("DISPLAY_NAME"), String::from("container-dev"))]), None);
        manager
            .register_provisioned_environment(
                provisioned_environment_id.clone(),
                handle,
                EnvironmentBag::new().with(EnvironmentAssertion::env_var("DISPLAY_NAME", "container-dev")),
                None,
            )
            .expect("register provisioned environment");

        let visible = manager.visible_environments().await;
        let ids: Vec<_> = visible
            .iter()
            .map(|environment| match environment {
                EnvironmentInfo::Direct { id, .. } | EnvironmentInfo::Provisioned { id, .. } => id.clone(),
            })
            .collect();

        assert_eq!(
            ids,
            vec![local_environment_id.clone(), provisioned_environment_id.clone(), ssh_environment_id.clone()],
            "visible environments should be sorted deterministically by id (with direct environments ordered before provisioned only when ids match)",
        );

        match &visible[0] {
            EnvironmentInfo::Direct { id, host_id, display_name, status } => {
                assert_eq!(id, &local_environment_id);
                assert_eq!(host_id.as_ref().map(HostId::as_str), Some("local-host-id"));
                assert_eq!(display_name.as_deref(), Some("local-dev"));
                assert_eq!(status, &EnvironmentStatus::Running);
            }
            other => panic!("expected local direct environment, got {other:?}"),
        }

        match &visible[1] {
            EnvironmentInfo::Provisioned { id, display_name, image, status } => {
                assert_eq!(id, &provisioned_environment_id);
                assert_eq!(display_name.as_deref(), Some("container-dev"));
                assert_eq!(image, &ImageId::new("mock:image"));
                assert_eq!(status, &EnvironmentStatus::Running);
            }
            other => panic!("expected provisioned environment, got {other:?}"),
        }

        match &visible[2] {
            EnvironmentInfo::Direct { id, host_id, display_name, status } => {
                assert_eq!(id, &ssh_environment_id);
                assert_eq!(host_id.as_ref().map(HostId::as_str), Some("ssh-host-id"));
                assert_eq!(display_name.as_deref(), Some("ssh-dev"));
                assert_eq!(status, &EnvironmentStatus::Running);
            }
            other => panic!("expected ssh direct environment, got {other:?}"),
        }
    }

    #[test]
    fn direct_environment_serialization_omits_image_metadata() {
        let info = EnvironmentInfo::Direct {
            id: EnvironmentId::new("direct-env"),
            host_id: Some(HostId::new("direct-host-id")),
            display_name: Some("ssh-dev".to_string()),
            status: EnvironmentStatus::Running,
        };

        let json = serde_json::to_value(&info).expect("serialize direct environment");
        let obj = json.as_object().expect("direct environment should serialize as a JSON object");

        assert_eq!(obj.get("kind").and_then(|value| value.as_str()), Some("direct"));
        assert_eq!(obj.get("id").and_then(|value| value.as_str()), Some("direct-env"));
        assert_eq!(obj.get("host_id").and_then(|value| value.as_str()), Some("direct-host-id"));
        assert!(obj.get("image").is_none(), "direct environments must not publish image metadata");
    }

    #[tokio::test]
    async fn register_direct_environment_rejects_local_collision() {
        let discovery = fake_discovery(false);
        let local_environment_id = test_local_environment_id();
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone(), test_local_host_id()).await;
        let result = manager.register_direct_environment(
            local_environment_id.clone(),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
            None,
        );

        assert!(result.is_err(), "local direct environment must not be replaceable through direct registration");
        assert!(manager.environment_bag(&local_environment_id).is_some());
    }

    #[tokio::test]
    async fn register_direct_environment_rejects_provisioned_collision() {
        let env_id = EnvironmentId::new("shared-environment");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let (handle, _) = mock_handle(&env_id, HashMap::new(), None);
        manager
            .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let result = manager.register_direct_environment(
            env_id.clone(),
            Arc::new(DiscoveryMockRunner::builder().build()),
            EnvironmentBag::new(),
            None,
        );

        assert!(result.is_err(), "existing provisioned environments must not be replaced by direct environments");
    }

    #[tokio::test]
    async fn update_direct_environment_bag_refreshes_lookup_state() {
        let discovery = fake_discovery(false);
        let local_environment_id = test_local_environment_id();
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone(), test_local_host_id()).await;
        let direct_environment_id = EnvironmentId::new("refreshable-direct-environment");

        manager
            .register_direct_environment(
                direct_environment_id.clone(),
                Arc::new(DiscoveryMockRunner::builder().build()),
                EnvironmentBag::new().with(EnvironmentAssertion::env_var("INITIAL", "true")),
                None,
            )
            .expect("register direct environment");

        manager
            .update_direct_environment_bag(
                &direct_environment_id,
                EnvironmentBag::new().with(EnvironmentAssertion::env_var("REFRESHED", "true")),
            )
            .expect("update direct environment bag");

        assert!(manager.environment_bag(&direct_environment_id).expect("direct bag").find_env_var("INITIAL").is_none());
        assert_eq!(manager.environment_bag(&direct_environment_id).expect("direct bag").find_env_var("REFRESHED"), Some("true"));
        assert!(manager.update_direct_environment_bag(&EnvironmentId::new("missing"), EnvironmentBag::new()).is_err());
        assert!(manager.environment_bag(&local_environment_id).is_some(), "local direct environment should remain registered");
    }

    #[tokio::test]
    async fn register_provisioned_environment_rejects_local_direct_collision() {
        let local_environment_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone(), test_local_host_id()).await;
        let original_bag = manager.environment_bag(&local_environment_id).expect("local environment bag");
        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: local_environment_id.clone(),
            image: ImageId::new("mock:image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
            provisioned_mounts: vec![],
            destroyed: Arc::new(AtomicBool::new(false)),
            destroy_error: None,
        });

        let result = manager.register_provisioned_environment(local_environment_id.clone(), handle, EnvironmentBag::new(), None);

        assert!(result.is_err(), "local direct environment must not be replaceable");
        assert!(manager.environment_registry(&local_environment_id).is_none());
        assert!(manager.environment_bag(&local_environment_id).is_some());
        assert!(manager.environment_bag(&local_environment_id).unwrap().find_binary("git").is_some());
        assert!(original_bag.find_binary("git").is_some());
    }

    #[tokio::test]
    async fn register_provisioned_environment_rejects_existing_direct_collision() {
        let local_environment_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone(), test_local_host_id()).await;
        let direct_env_id = EnvironmentId::new("direct-environment");
        let direct_bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("DIRECT", "true"));
        let direct_runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;

        {
            let mut managed = manager.managed.lock().expect("environment manager lock poisoned");
            managed.insert(
                direct_env_id.clone(),
                ManagedEnvironmentKind::Direct(DirectEnvironmentState {
                    runner: direct_runner,
                    env_bag: direct_bag.clone(),
                    host_id: Some(HostId::new("direct-host-id")),
                    display_name: Some("direct-env".to_string()),
                }),
            );
        }

        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: direct_env_id.clone(),
            image: ImageId::new("mock:image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
            provisioned_mounts: vec![],
            destroyed: Arc::new(AtomicBool::new(false)),
            destroy_error: None,
        });

        let result = manager.register_provisioned_environment(direct_env_id.clone(), handle, EnvironmentBag::new(), None);

        assert!(result.is_err(), "existing direct environments must not be replaced");
        assert_eq!(manager.environment_bag(&direct_env_id).expect("direct env bag").find_env_var("DIRECT"), Some("true"));
        assert!(manager.environment_registry(&direct_env_id).is_none());
    }

    #[tokio::test]
    async fn create_provisioned_environment_registers_handle() {
        let env_id = EnvironmentId::new("env-created-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let (handle, _) = mock_handle(&env_id, HashMap::new(), None);
        let provider = Arc::new(MockEnvironmentProvider { create_result: tokio::sync::Mutex::new(Some(Ok(handle))) });
        let mut registry = ProviderRegistry::new();
        registry.environment_providers.insert(
            "docker",
            crate::providers::discovery::ProviderDescriptor::named(
                crate::providers::discovery::ProviderCategory::EnvironmentProvider,
                "docker",
            ),
            provider,
        );

        manager
            .create_provisioned_environment(CreateProvisionedEnvironmentRequest {
                env_id: env_id.clone(),
                provider: "docker",
                registry: &registry,
                image: ImageId::new("mock:image"),
                tokens: vec![],
                config_base: &DaemonHostPath::new("/tmp/test-config"),
                daemon_socket_path: &DaemonHostPath::new("/tmp/flotilla.sock"),
                reference_repo: None,
            })
            .await
            .expect("create provisioned environment");

        assert!(manager.environment_runner(&env_id).is_some());
        assert!(manager.environment_registry(&env_id).is_some());
    }

    #[tokio::test]
    async fn ensure_provisioned_environment_providers_updates_bag_and_registry() {
        let env_id = EnvironmentId::new("env-discover-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let (handle, _) = mock_handle(&env_id, HashMap::from([(String::from("ANTHROPIC_API_KEY"), String::from("test-key"))]), None);
        manager
            .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        manager
            .ensure_provisioned_environment_providers(&env_id, &DaemonHostPath::new("/tmp/test-config"))
            .await
            .expect("ensure providers");

        assert_eq!(manager.environment_bag(&env_id).expect("environment bag").find_env_var("ANTHROPIC_API_KEY"), Some("test-key"));
        assert!(manager.environment_registry(&env_id).is_some());
    }

    #[tokio::test]
    async fn provisioned_mount_helpers_translate_paths_for_the_owning_environment() {
        let env_id = EnvironmentId::new("env-mount-translate-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: env_id.clone(),
            image: ImageId::new("mock:image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
            provisioned_mounts: vec![reference_repo_mount()],
            destroyed: Arc::new(AtomicBool::new(false)),
            destroy_error: None,
        });
        manager
            .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        assert!(manager.provisioned_mounts(&EnvironmentId::new("missing")).is_none());
        assert!(manager
            .resolve_environment_path_to_host_path(
                &EnvironmentId::new("missing"),
                &crate::path_context::ExecutionEnvironmentPath::new("/ref/repo")
            )
            .is_none());

        let host_path = manager
            .resolve_environment_path_to_host_path(&env_id, &crate::path_context::ExecutionEnvironmentPath::new("/ref/repo/subdir"))
            .expect("host path");
        assert_eq!(host_path.to_string(), format!("host:{}:/host/reference-repo/subdir", test_local_host_id()));

        let env_path = manager.resolve_host_path_to_environment_path(&env_id, &host_path).expect("environment path");
        assert_eq!(env_path, crate::path_context::ExecutionEnvironmentPath::new("/ref/repo/subdir"));

        assert!(
            manager
                .resolve_environment_path_to_host_path(&env_id, &crate::path_context::ExecutionEnvironmentPath::new("/ref/repo/../other"))
                .is_none(),
            "escaping parent components must be rejected"
        );
        assert!(
            manager
                .resolve_host_path_to_environment_path(
                    &env_id,
                    &flotilla_protocol::qualified_path::QualifiedPath::host(test_local_host_id(), "/host/reference-repo/./other")
                )
                .is_none(),
            "dot components in host paths must be rejected"
        );
    }

    #[tokio::test]
    async fn destroy_provisioned_environment_unregisters_and_destroys_handle() {
        let env_id = EnvironmentId::new("env-destroy-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let (handle, destroyed) = mock_handle(&env_id, HashMap::new(), None);
        manager
            .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        manager.destroy_provisioned_environment(&env_id).await.expect("destroy provisioned environment");

        assert!(destroyed.load(Ordering::SeqCst));
        assert!(manager.environment_runner(&env_id).is_none());
        assert!(manager.environment_registry(&env_id).is_none());
    }

    #[tokio::test]
    async fn destroy_provisioned_environment_keeps_state_when_destroy_fails() {
        let env_id = EnvironmentId::new("env-destroy-fail-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let (handle, destroyed) = mock_handle(&env_id, HashMap::new(), Some("boom".to_string()));
        manager
            .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let result = manager.destroy_provisioned_environment(&env_id).await;

        assert_eq!(result.unwrap_err(), "boom");
        assert!(destroyed.load(Ordering::SeqCst));
        assert!(manager.environment_runner(&env_id).is_some(), "failed destroy should preserve manager state");
    }

    #[tokio::test]
    async fn discovery_update_does_not_resurrect_environment_after_concurrent_destroy() {
        let env_id = EnvironmentId::new("env-discovery-race-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
        let (handle, _) = mock_handle(&env_id, HashMap::new(), None);
        manager
            .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        let snapped = manager.provisioned_environment(&env_id).expect("snapshot provisioned state");
        let removed = manager.remove_provisioned_environment(&env_id);
        assert!(removed.is_some(), "destroy path should remove the environment first");

        manager.update_provisioned_environment_discovery(
            &env_id,
            &snapped.handle,
            EnvironmentBag::new().with(EnvironmentAssertion::env_var("RACE", "stale")),
            Some(Arc::new(ProviderRegistry::new())),
        );

        assert!(manager.environment_runner(&env_id).is_none(), "stale discovery must not resurrect a removed environment");
        assert!(manager.environment_bag(&env_id).is_none());
        assert!(manager.environment_registry(&env_id).is_none());
    }

    #[tokio::test]
    async fn from_local_state_uses_supplied_runner_and_bag() {
        let env_id = test_local_environment_id();
        let runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("SEEDED", "true"));

        let manager = EnvironmentManager::from_local_state(env_id.clone(), test_local_host_id(), Arc::clone(&runner), bag.clone());

        assert!(Arc::ptr_eq(&manager.environment_runner(&env_id).expect("runner"), &runner));
        assert_eq!(manager.environment_bag(&env_id).expect("bag").find_env_var("SEEDED"), Some("true"));
    }
}
