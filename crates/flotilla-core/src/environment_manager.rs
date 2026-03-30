use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{CommandValue, EnvironmentId, EnvironmentInfo, EnvironmentStatus};

use crate::{
    config::ConfigStore,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        discovery::{run_host_detectors, DiscoveryRuntime, EnvironmentAssertion, EnvironmentBag, FactoryRegistry},
        environment::{CreateOpts, EnvironmentHandle},
        registry::ProviderRegistry,
        CommandRunner,
    },
    step::StepOutcome,
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
}

#[derive(Clone)]
pub struct ProvisionedEnvironmentState {
    pub handle: EnvironmentHandle,
    pub env_bag: EnvironmentBag,
    pub registry: Option<Arc<ProviderRegistry>>,
}

pub struct EnvironmentManager {
    local_environment_id: EnvironmentId,
    host_runner: Arc<dyn CommandRunner>,
    managed: Mutex<HashMap<EnvironmentId, ManagedEnvironmentKind>>,
}

pub struct CreateProvisionedEnvironmentRequest<'a> {
    pub env_id: EnvironmentId,
    pub provider: &'a str,
    pub registry: &'a ProviderRegistry,
    pub daemon_socket_path: &'a DaemonHostPath,
    pub reference_repo: Option<DaemonHostPath>,
    pub prior: &'a [StepOutcome],
}

impl EnvironmentManager {
    pub async fn new_local(discovery: &DiscoveryRuntime, local_environment_id: EnvironmentId) -> Self {
        let env_bag = run_host_detectors(&discovery.host_detectors, &*discovery.runner, &*discovery.env).await;
        Self::from_local_state(local_environment_id, Arc::clone(&discovery.runner), env_bag)
    }

    pub fn from_local_state(local_environment_id: EnvironmentId, local_runner: Arc<dyn CommandRunner>, env_bag: EnvironmentBag) -> Self {
        let mut managed = HashMap::new();
        managed.insert(
            local_environment_id.clone(),
            ManagedEnvironmentKind::Direct(DirectEnvironmentState { runner: Arc::clone(&local_runner), env_bag }),
        );

        Self { local_environment_id, host_runner: local_runner, managed: Mutex::new(managed) }
    }

    pub fn local_environment_id(&self) -> &EnvironmentId {
        &self.local_environment_id
    }

    pub fn local_environment_bag(&self) -> EnvironmentBag {
        self.environment_bag(&self.local_environment_id).expect("local direct environment must be registered in EnvironmentManager")
    }

    pub fn environment_runner(&self, env_id: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        match self.managed_environment(env_id)? {
            ManagedEnvironmentKind::Direct(state) => Some(state.runner),
            ManagedEnvironmentKind::Provisioned(state) => Some(state.handle.runner(Arc::clone(&self.host_runner))),
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

    pub async fn host_summary_environments(&self) -> Vec<EnvironmentInfo> {
        let provisioned: Vec<_> = {
            let managed = self.managed.lock().expect("environment manager lock poisoned");
            managed
                .values()
                .filter_map(|state| match state {
                    ManagedEnvironmentKind::Direct(_) => None,
                    ManagedEnvironmentKind::Provisioned(state) => Some(Arc::clone(&state.handle)),
                })
                .collect()
        };

        let mut environments = Vec::with_capacity(provisioned.len());
        for handle in provisioned {
            let status = handle.status().await.unwrap_or_else(EnvironmentStatus::Failed);
            environments.push(EnvironmentInfo { id: handle.id().clone(), image: handle.image().clone(), status });
        }
        environments.sort_by(|a, b| a.id.cmp(&b.id));
        environments
    }

    pub async fn create_provisioned_environment(&self, request: CreateProvisionedEnvironmentRequest<'_>) -> Result<(), String> {
        let CreateProvisionedEnvironmentRequest { env_id, provider, registry, daemon_socket_path, reference_repo, prior } = request;
        let image = prior
            .iter()
            .find_map(|outcome| match outcome {
                StepOutcome::Produced(CommandValue::ImageEnsured { image }) => Some(image.clone()),
                _ => None,
            })
            .ok_or_else(|| "image not produced by prior EnsureEnvironmentImage step".to_string())?;

        let (_, env_provider) =
            registry.environment_providers.get(provider).ok_or_else(|| format!("environment provider not available: {provider}"))?;

        let tokens = prior
            .iter()
            .find_map(|outcome| match outcome {
                StepOutcome::Produced(CommandValue::EnvironmentSpecRead { spec }) => Some(spec),
                _ => None,
            })
            .map(|spec| {
                spec.token_env_vars
                    .iter()
                    .filter_map(|name| match std::env::var(name) {
                        Ok(val) => Some((name.clone(), val)),
                        Err(_) => {
                            tracing::warn!(env_var = %name, "token env var not set on host, skipping");
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let opts = CreateOpts { tokens, reference_repo, daemon_socket_path: daemon_socket_path.clone(), working_directory: None };
        let handle = env_provider.create(env_id.clone(), &image, opts).await?;
        self.register_provisioned_environment(env_id, handle, EnvironmentBag::new(), None)
    }

    pub async fn discover_provisioned_environment_providers(
        &self,
        env_id: &EnvironmentId,
        config_base: &DaemonHostPath,
    ) -> Result<(), String> {
        let state = self.provisioned_environment(env_id)?;
        let env_runner = state.handle.runner(Arc::clone(&self.host_runner));

        let raw_env_vars = state.handle.env_vars().await?;
        let mut bag = EnvironmentBag::new();
        for (key, value) in &raw_env_vars {
            bag = bag.with(EnvironmentAssertion::env_var(key, value));
        }

        let config = ConfigStore::with_base(config_base.as_path().join(format!("env-discovery/{env_id}")));
        let env_repo_root = ExecutionEnvironmentPath::new("/workspace");
        let provider_registry = FactoryRegistry::default_all().probe_all(&bag, &config, &env_repo_root, env_runner).await;

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
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.entry(env_id.clone()) {
            Entry::Occupied(mut entry) => {
                if matches!(entry.get(), ManagedEnvironmentKind::Direct(_)) {
                    return Err(format!("cannot replace direct environment {env_id} with a provisioned environment"));
                }
                entry.insert(ManagedEnvironmentKind::Provisioned(ProvisionedEnvironmentState { handle, env_bag, registry }));
            }
            Entry::Vacant(entry) => {
                entry.insert(ManagedEnvironmentKind::Provisioned(ProvisionedEnvironmentState { handle, env_bag, registry }));
            }
        }
        Ok(())
    }

    pub fn remove_provisioned_environment(&self, env_id: &EnvironmentId) -> Option<ProvisionedEnvironmentState> {
        if env_id == &self.local_environment_id {
            return None;
        }

        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.remove(env_id) {
            Some(ManagedEnvironmentKind::Provisioned(state)) => Some(state),
            Some(ManagedEnvironmentKind::Direct(state)) => {
                managed.insert(env_id.clone(), ManagedEnvironmentKind::Direct(state));
                None
            }
            None => None,
        }
    }

    fn managed_environment(&self, env_id: &EnvironmentId) -> Option<ManagedEnvironmentKind> {
        self.managed.lock().expect("environment manager lock poisoned").get(env_id).cloned()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_local_environment_bag_for_test(&self, env_bag: EnvironmentBag) -> Result<(), String> {
        let mut managed = self.managed.lock().expect("environment manager lock poisoned");
        match managed.get_mut(&self.local_environment_id) {
            Some(ManagedEnvironmentKind::Direct(state)) => {
                state.env_bag = env_bag;
                Ok(())
            }
            Some(ManagedEnvironmentKind::Provisioned(_)) => Err("local environment unexpectedly registered as provisioned".into()),
            None => Err("local environment missing from manager".into()),
        }
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
    use flotilla_protocol::{CommandValue, EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};

    use super::*;
    use crate::{
        providers::{
            discovery::{
                test_support::{fake_discovery, DiscoveryMockRunner},
                EnvironmentAssertion,
            },
            environment::{CreateOpts, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment},
            registry::ProviderRegistry,
            CommandRunner,
        },
        step::StepOutcome,
    };

    struct MockProvisionedEnvironment {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
        env_vars: HashMap<String, String>,
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

        fn runner(&self, _host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner> {
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
            destroyed: Arc::clone(&destroyed),
            destroy_error,
        });
        (handle, destroyed)
    }

    fn test_local_environment_id() -> EnvironmentId {
        EnvironmentId::new("test-local-environment")
    }

    #[tokio::test]
    async fn new_local_registers_direct_environment() {
        let env_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, env_id.clone()).await;

        assert_eq!(manager.local_environment_id(), &env_id);
        assert!(manager.environment_runner(&env_id).is_some());
        assert!(manager.environment_bag(&env_id).is_some());
        assert!(manager.environment_runner(&EnvironmentId::new("missing")).is_none());
    }

    #[tokio::test]
    async fn direct_environment_lookup_uses_host_detectors() {
        let env_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, env_id.clone()).await;

        let bag = manager.environment_bag(&env_id).expect("local environment bag");
        assert!(bag.find_binary("git").is_some(), "host detectors should populate the direct environment bag");
    }

    #[tokio::test]
    async fn register_and_remove_provisioned_environment_updates_state() {
        let env_id = EnvironmentId::new("env-provisioned-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id()).await;
        let runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;
        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: env_id.clone(),
            image: ImageId::new("mock:image"),
            runner,
            env_vars: HashMap::new(),
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

        let lookup_registry = manager.environment_registry(&env_id).expect("provisioned environment registry");
        assert!(Arc::ptr_eq(&lookup_registry, &registry));

        let removed = manager.remove_provisioned_environment(&env_id).expect("provisioned environment removed");
        assert_eq!(removed.env_bag.find_env_var("PROVISIONED"), Some("true"));
        assert!(manager.environment_bag(&env_id).is_none());
        assert!(manager.environment_registry(&env_id).is_none());
    }

    #[tokio::test]
    async fn register_provisioned_environment_rejects_local_direct_collision() {
        let local_environment_id = test_local_environment_id();
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone()).await;
        let original_bag = manager.environment_bag(&local_environment_id).expect("local environment bag");
        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: local_environment_id.clone(),
            image: ImageId::new("mock:image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
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
        let manager = EnvironmentManager::new_local(&discovery, local_environment_id.clone()).await;
        let direct_env_id = EnvironmentId::new("direct-environment");
        let direct_bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("DIRECT", "true"));
        let direct_runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;

        {
            let mut managed = manager.managed.lock().expect("environment manager lock poisoned");
            managed.insert(
                direct_env_id.clone(),
                ManagedEnvironmentKind::Direct(DirectEnvironmentState { runner: direct_runner, env_bag: direct_bag.clone() }),
            );
        }

        let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
            id: direct_env_id.clone(),
            image: ImageId::new("mock:image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::new(),
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
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id()).await;
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
                daemon_socket_path: &DaemonHostPath::new("/tmp/flotilla.sock"),
                reference_repo: None,
                prior: &[
                    StepOutcome::Produced(CommandValue::ImageEnsured { image: ImageId::new("mock:image") }),
                    StepOutcome::Produced(CommandValue::EnvironmentSpecRead {
                        spec: EnvironmentSpec { image: ImageSource::Registry("mock:image".into()), token_env_vars: vec![] },
                    }),
                ],
            })
            .await
            .expect("create provisioned environment");

        assert!(manager.environment_runner(&env_id).is_some());
        assert!(manager.environment_registry(&env_id).is_none());
    }

    #[tokio::test]
    async fn discover_provisioned_environment_providers_updates_bag_and_registry() {
        let env_id = EnvironmentId::new("env-discover-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id()).await;
        let (handle, _) = mock_handle(&env_id, HashMap::from([(String::from("ANTHROPIC_API_KEY"), String::from("test-key"))]), None);
        manager
            .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
            .expect("register provisioned environment");

        manager
            .discover_provisioned_environment_providers(&env_id, &DaemonHostPath::new("/tmp/test-config"))
            .await
            .expect("discover providers");

        assert_eq!(manager.environment_bag(&env_id).expect("environment bag").find_env_var("ANTHROPIC_API_KEY"), Some("test-key"));
        assert!(manager.environment_registry(&env_id).is_some());
    }

    #[tokio::test]
    async fn destroy_provisioned_environment_unregisters_and_destroys_handle() {
        let env_id = EnvironmentId::new("env-destroy-1");
        let discovery = fake_discovery(false);
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id()).await;
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
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id()).await;
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
        let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id()).await;
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

        let manager = EnvironmentManager::from_local_state(env_id.clone(), Arc::clone(&runner), bag.clone());

        assert!(Arc::ptr_eq(&manager.environment_runner(&env_id).expect("runner"), &runner));
        assert_eq!(manager.environment_bag(&env_id).expect("bag").find_env_var("SEEDED"), Some("true"));
    }
}
