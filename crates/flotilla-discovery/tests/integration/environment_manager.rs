use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use flotilla_environment::environment::PrepareOpts;
use flotilla_environment::environment::ProvisionOpts;
use flotilla_protocol::{EnvironmentId, EnvironmentInfo, EnvironmentStatus, ImageId};
use tokio::sync::Notify;

use flotilla_discovery::detectors::default_host_detectors;
use flotilla_discovery::environment_manager::EnvironmentManager;
use flotilla_discovery::environments::CreateProvisionedEnvironmentRequest;
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_discovery_api::EnvironmentAssertion;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_discovery_testkit::DiscoveryMockRunner;
use flotilla_environment::environment::EnvironmentHandle;
use flotilla_environment::environment::EnvironmentProvider;
use flotilla_environment::environment::PreparedEnvironment;
use flotilla_environment::environment::ProvisionedEnvironment;
use flotilla_environment::environment::ProvisionedMount;
use flotilla_environment::environment::ProvisionedMountMode;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::qualified_path::HostId;
use flotilla_provider_runtime::CommandRunner;

struct MockProvisionedEnvironment {
    id: EnvironmentId,
    image: ImageId,
    runner: Arc<dyn CommandRunner>,
    env_vars: HashMap<String, String>,
    provisioned_mounts: Vec<ProvisionedMount>,
    destroyed: Arc<AtomicBool>,
    destroy_error: Option<String>,
    // (entered, release): env_vars signals entry, then waits for release.
    env_vars_gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

#[async_trait]
impl ProvisionedEnvironment for MockProvisionedEnvironment {
    flotilla_discovery_testkit::injected_environment_handle!();

    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
    }

    fn container_name(&self) -> Option<&str> {
        Some("mock-container")
    }

    async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
        flotilla_environment::environment::ProviderObservation::new(flotilla_environment::environment::ObservationState::Running)
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        if let Some((entered, release)) = &self.env_vars_gate {
            entered.notify_one();
            release.notified().await;
        }
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
    flotilla_discovery_testkit::injected_environment_provider!();
    async fn prepare(&self, _spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        Ok(PreparedEnvironment::new(&Arc::new(()), ()))
    }

    async fn provision(&self, _id: EnvironmentId, _image: &PreparedEnvironment, _opts: ProvisionOpts) -> Result<EnvironmentHandle, String> {
        self.create_result.lock().await.take().expect("create called more than expected")
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(vec![])
    }

    async fn destroy(&self, _container_id: &str) -> Result<(), String> {
        Err("unused in test".to_string())
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
        env_vars_gate: None,
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
    ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro)
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

    // #2503: the local resource reference aliases the runner, without adding
    // a direct registration that could collide with the daemon's Host.
    let local_ref = format!("host-direct-{}", test_local_host_id());
    let resolved = manager.resolve_environment_ref(&local_ref).expect("local resource environment");
    assert_eq!(resolved.id, env_id);
    assert!(Arc::ptr_eq(&resolved.runner, &discovery.runner));
    assert!(!manager.managed_environments().iter().any(|(id, _)| id.as_str() == local_ref));
    assert_eq!(manager.managed_environments().len(), 1);
    // Unknown, empty and near-matching references must never fall back locally.
    for reference in ["", "missing", "host-direct-test-local-host-id-extra", "host-direct-remote"] {
        assert!(manager.resolve_environment_ref(reference).is_none(), "unexpected runner for {reference}");
    }
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
    assert!(manager.provisioned_mounts(&test_local_environment_id()).is_none(), "direct environments must not expose provisioned mounts");
    let runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;
    let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
        id: env_id.clone(),
        image: ImageId::new("mock:image"),
        runner,
        env_vars: HashMap::new(),
        provisioned_mounts: vec![reference_repo_mount()],
        destroyed: Arc::new(AtomicBool::new(false)),
        destroy_error: None,
        env_vars_gate: None,
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
        Arc::new(vec![]),
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
        vec![
            EnvironmentId::host(HostId::new("local-host-id")),
            EnvironmentId::host(HostId::new("ssh-host-id")),
            provisioned_environment_id.clone(),
        ],
        "visible environments should be sorted deterministically by canonical environment id",
    );

    match &visible[0] {
        EnvironmentInfo::Direct { id, host_id, display_name, status } => {
            assert_eq!(id, &EnvironmentId::host(HostId::new("local-host-id")));
            assert_eq!(host_id.as_ref().map(HostId::as_str), Some("local-host-id"));
            assert_eq!(display_name.as_deref(), Some("local-dev"));
            assert_eq!(status, &EnvironmentStatus::Running);
        }
        other => panic!("expected local direct environment, got {other:?}"),
    }

    match &visible[1] {
        EnvironmentInfo::Direct { id, host_id, display_name, status } => {
            assert_eq!(id, &EnvironmentId::host(HostId::new("ssh-host-id")));
            assert_eq!(host_id.as_ref().map(HostId::as_str), Some("ssh-host-id"));
            assert_eq!(display_name.as_deref(), Some("ssh-dev"));
            assert_eq!(status, &EnvironmentStatus::Running);
        }
        other => panic!("expected ssh direct environment, got {other:?}"),
    }

    match &visible[2] {
        EnvironmentInfo::Provisioned { id, display_name, image, status } => {
            assert_eq!(id, &provisioned_environment_id);
            assert_eq!(display_name.as_deref(), Some("container-dev"));
            assert_eq!(image, &ImageId::new("mock:image"));
            assert_eq!(status, &EnvironmentStatus::Running);
        }
        other => panic!("expected provisioned environment, got {other:?}"),
    }
}

#[test]
fn direct_environment_serialization_omits_image_metadata() {
    let info = EnvironmentInfo::Direct {
        id: EnvironmentId::host(HostId::new("direct-host-id")),
        host_id: Some(HostId::new("direct-host-id")),
        display_name: Some("ssh-dev".to_string()),
        status: EnvironmentStatus::Running,
    };

    let json = serde_json::to_value(&info).expect("serialize direct environment");
    let obj = json.as_object().expect("direct environment should serialize as a JSON object");

    assert_eq!(obj.get("kind").and_then(|value| value.as_str()), Some("direct"));
    assert_eq!(obj.get("id").and_then(|value| value.as_str()), Some("host:direct-host-id"));
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

    let result =
        manager.register_direct_environment(env_id.clone(), Arc::new(DiscoveryMockRunner::builder().build()), EnvironmentBag::new(), None);

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
        env_vars_gate: None,
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

    manager
        .register_direct_environment(direct_env_id.clone(), direct_runner, direct_bag.clone(), Some(HostId::new("direct-host-id")))
        .expect("register direct environment");

    let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
        id: direct_env_id.clone(),
        image: ImageId::new("mock:image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::new(),
        provisioned_mounts: vec![],
        destroyed: Arc::new(AtomicBool::new(false)),
        destroy_error: None,
        env_vars_gate: None,
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
    registry.environment_providers.insert("docker", ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker"), provider);

    manager
        .create_provisioned_environment(CreateProvisionedEnvironmentRequest {
            env_id: env_id.clone(),
            provider: "docker",
            registry: &registry,
            prepared: PreparedEnvironment::new(&Arc::new(()), ()),
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

// #1131: configured detectors must run inside both newly created and
// adopted environments, even after the runtime has been dropped.
// Glue: the same custom command detector exercises both probe callers.
#[tokio::test]
async fn provisioned_discovery_retains_configured_detectors() {
    use flotilla_discovery_api::detectors::parse_first_dotted_version;
    use flotilla_discovery_api::detectors::CommandDetector;

    let mut discovery = fake_discovery(false);
    discovery.host_detectors = Arc::new(vec![Box::new(CommandDetector::new("custom-tool", &["--version"], parse_first_dotted_version))]);
    discovery.runner = Arc::new(DiscoveryMockRunner::builder().on_run("custom-tool", &["--version"], Ok("custom 1.0.0".into())).build());
    let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
    assert!(manager.local_environment_bag().assertions().contains(&EnvironmentAssertion::versioned_binary(
        "custom-tool",
        "custom-tool",
        "1.0.0"
    )));
    drop(discovery);

    let env_id = EnvironmentId::new("custom-provisioned");
    // The fake runner stands in for the provisioned subprocess boundary.
    // Responses are consumed: custom-tool needs one response for creation
    // and another for the later ensure probe. Keep both registrations.
    let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
        id: env_id.clone(),
        image: ImageId::new("mock:image"),
        runner: Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("custom-tool", &["--version"], Ok("custom 2.0.0".into()))
                .on_run("custom-tool", &["--version"], Ok("custom 2.0.0".into()))
                .on_run("codex", &["--version"], Ok("codex-cli 1.2.3".into()))
                .build(),
        ),
        env_vars: HashMap::new(),
        provisioned_mounts: vec![],
        destroyed: Arc::new(AtomicBool::new(false)),
        destroy_error: None,
        env_vars_gate: None,
    });
    let mut registry = ProviderRegistry::new();
    registry.environment_providers.insert(
        "mock",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "mock"),
        Arc::new(MockEnvironmentProvider { create_result: tokio::sync::Mutex::new(Some(Ok(handle))) }),
    );
    manager
        .create_provisioned_environment(CreateProvisionedEnvironmentRequest {
            env_id: env_id.clone(),
            provider: "mock",
            registry: &registry,
            prepared: PreparedEnvironment::new(&Arc::new(()), ()),
            tokens: vec![],
            config_base: &DaemonHostPath::new("/tmp/test-config"),
            daemon_socket_path: &DaemonHostPath::new("/tmp/flotilla.sock"),
            reference_repo: None,
        })
        .await
        .expect("create environment");
    let bag = manager.environment_bag(&env_id).unwrap();
    assert!(bag.assertions().contains(&EnvironmentAssertion::versioned_binary("custom-tool", "custom-tool", "2.0.0")));
    assert!(bag.find_binary("codex").is_none(), "unconfigured defaults must not run");
    // Clear discovery so ensure cannot return the already-populated registry.
    let state = manager.remove_provisioned_environment(&env_id).unwrap();
    manager.register_provisioned_environment(env_id.clone(), state.handle, EnvironmentBag::new(), None).unwrap();
    manager
        .ensure_provisioned_environment_providers(&env_id, &DaemonHostPath::new("/tmp/test-config"))
        .await
        .expect("discover adopted environment");
    let bag = manager.environment_bag(&env_id).unwrap();
    assert!(bag.assertions().contains(&EnvironmentAssertion::versioned_binary("custom-tool", "custom-tool", "2.0.0")));
    assert!(bag.find_binary("codex").is_none(), "unconfigured defaults must not run");
}

#[tokio::test]
async fn ensure_provisioned_environment_providers_updates_bag_and_registry() {
    let env_id = EnvironmentId::new("env-discover-1");
    let mut discovery = fake_discovery(false);
    discovery.host_detectors = Arc::new(default_host_detectors());
    let manager = EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await;
    let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
        id: env_id.clone(),
        image: ImageId::new("mock:image"),
        runner: Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("codex", &["--version"], Ok("codex-cli 1.2.3".to_string()))
                .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("/usr/local/bin/codex\n".to_string()))
                .build(),
        ),
        env_vars: HashMap::from([
            (String::from("ANTHROPIC_API_KEY"), String::from("test-key")),
            (String::from("FLOTILLA_ENVIRONMENT_ID"), String::from("env-discover-1")),
        ]),
        provisioned_mounts: Vec::new(),
        destroyed: Arc::new(AtomicBool::new(false)),
        destroy_error: None,
        env_vars_gate: None,
    });
    manager
        .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
        .expect("register provisioned environment");

    manager.ensure_provisioned_environment_providers(&env_id, &DaemonHostPath::new("/tmp/test-config")).await.expect("ensure providers");

    assert_eq!(manager.environment_bag(&env_id).expect("environment bag").find_env_var("ANTHROPIC_API_KEY"), Some("test-key"));
    assert_eq!(manager.environment_bag(&env_id).expect("environment bag").find_env_var("FLOTILLA_ENVIRONMENT_ID"), Some("env-discover-1"));
    assert!(manager.environment_registry(&env_id).expect("environment registry").agent_adapters.get("codex").is_some());
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
        env_vars_gate: None,
    });
    manager
        .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
        .expect("register provisioned environment");

    assert!(manager.provisioned_mounts(&EnvironmentId::new("missing")).is_none());
    assert!(manager
        .resolve_environment_path_to_host_path(
            &EnvironmentId::new("missing"),
            &flotilla_paths::path_context::ExecutionEnvironmentPath::new("/ref/repo")
        )
        .is_none());

    let host_path = manager
        .resolve_environment_path_to_host_path(&env_id, &flotilla_paths::path_context::ExecutionEnvironmentPath::new("/ref/repo/subdir"))
        .expect("host path");
    assert_eq!(host_path.to_string(), format!("host:{}:/host/reference-repo/subdir", test_local_host_id()));

    let env_path = manager.resolve_host_path_to_environment_path(&env_id, &host_path).expect("environment path");
    assert_eq!(env_path, flotilla_paths::path_context::ExecutionEnvironmentPath::new("/ref/repo/subdir"));

    assert!(
        manager
            .resolve_environment_path_to_host_path(
                &env_id,
                &flotilla_paths::path_context::ExecutionEnvironmentPath::new("/ref/repo/../other")
            )
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
    let manager = Arc::new(EnvironmentManager::new_local(&discovery, test_local_environment_id(), test_local_host_id()).await);
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let handle: EnvironmentHandle = Arc::new(MockProvisionedEnvironment {
        id: env_id.clone(),
        image: ImageId::new("mock:image"),
        runner: Arc::new(DiscoveryMockRunner::builder().build()),
        env_vars: HashMap::from([("RACE".to_string(), "stale".to_string())]),
        provisioned_mounts: vec![],
        destroyed: Arc::new(AtomicBool::new(false)),
        destroy_error: None,
        env_vars_gate: Some((Arc::clone(&entered), Arc::clone(&release))),
    });
    manager
        .register_provisioned_environment(env_id.clone(), handle, EnvironmentBag::new(), None)
        .expect("register provisioned environment");

    // Discovery snapshots the provisioned state, then probes; destroy removes
    // the environment while the probe is in flight.
    let probe = tokio::spawn({
        let manager = Arc::clone(&manager);
        let env_id = env_id.clone();
        async move { manager.ensure_provisioned_environment_providers(&env_id, &DaemonHostPath::new("/tmp/test-config")).await }
    });
    entered.notified().await;
    let removed = manager.remove_provisioned_environment(&env_id);
    assert!(removed.is_some(), "destroy path should remove the environment first");
    release.notify_one();
    probe.await.expect("probe task").expect("stale discovery completes without error");

    assert!(manager.environment_runner(&env_id).is_none(), "stale discovery must not resurrect a removed environment");
    assert!(manager.environment_bag(&env_id).is_none());
    assert!(manager.environment_registry(&env_id).is_none());
}

#[tokio::test]
async fn from_local_state_uses_supplied_runner_and_bag() {
    let env_id = test_local_environment_id();
    let runner = Arc::new(DiscoveryMockRunner::builder().build()) as Arc<dyn CommandRunner>;
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("SEEDED", "true"));

    let manager =
        EnvironmentManager::from_local_state(env_id.clone(), test_local_host_id(), Arc::clone(&runner), bag.clone(), Arc::new(vec![]));

    assert!(Arc::ptr_eq(&manager.environment_runner(&env_id).expect("runner"), &runner));
    assert_eq!(manager.environment_bag(&env_id).expect("bag").find_env_var("SEEDED"), Some("true"));
}
