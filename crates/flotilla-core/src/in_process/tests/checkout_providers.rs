use flotilla_store_memory::InMemoryBackend;
use std::sync::Arc;
use std::time::Duration;

use flotilla_change_request::ChangeRequestTracker;
use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, ObservedCheckoutSpec as ResourceObservedCheckoutSpec, Repository,
    RepositorySpec,
};
use flotilla_store::ResourceBackend;

use super::support::test_meta;
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::{fake_discovery, FakeChangeRequest, FakeVcsFactory, FakeVcsState};
use flotilla_admission::convoy_admission::RepositoryChangeRequestProvider;

#[tokio::test]
async fn checkout_vcs_discovery_is_cached_per_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"checkout-vcs-cache-test\"\n").expect("daemon config");
    let first_path = temp.path().join("first");
    let second_path = temp.path().join("second");
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![
        Box::new(FakeVcsFactory::new(FakeVcsState::builder(&first_path).build())),
        Box::new(FakeVcsFactory::new(FakeVcsState::builder(&second_path).build())),
    ];
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("local-host"),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;

    // #1770: only observed Checkout lifetimes retain discovered providers.
    for (name, path) in [("first", &first_path), ("second", &second_path)] {
        let repository = RepositorySpec::remote(format!("https://github.com/example/{name}")).expect("repository");
        daemon
            .resource_backend
            .using::<Repository>("flotilla")
            .create(&test_meta(&repository.key().to_string()), &repository)
            .await
            .expect("repository");
        daemon
            .observed_resource_backend
            .clone()
            .using::<ResourceCheckout>("flotilla")
            .create(
                &test_meta(name),
                &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                    r#ref: "main".into(),
                    path: path.to_string_lossy().into(),
                    repo_ref: repository.key(),
                    host_ref: daemon.environment_manager.local_host_id().to_string(),
                    is_main: true,
                }),
            )
            .await
            .expect("observed checkout");
    }

    let first = daemon.local_vcs_for_checkout(&first_path).await.expect("first checkout VCS");
    let first_again = daemon.local_vcs_for_checkout(&first_path).await.expect("cached first checkout VCS");
    let second = daemon.local_vcs_for_checkout(&second_path).await.expect("second checkout VCS");

    assert!(Arc::ptr_eq(&first, &first_again));
    assert!(!Arc::ptr_eq(&first, &second));
}
/// Claims a path only when its bag names the remote on `forge.example`.
struct RemoteClaiming(FakeVcsFactory);

#[async_trait::async_trait]
impl flotilla_discovery_api::Factory for RemoteClaiming {
    type Descriptor = flotilla_discovery_api::ProviderDescriptor;
    type Output = dyn flotilla_vcs::vcs::Vcs;

    fn descriptor(&self) -> Self::Descriptor {
        self.0.descriptor()
    }

    async fn probe(
        &self,
        env: &flotilla_discovery_api::EnvironmentBag,
        config: &dyn flotilla_discovery_api::provider_config::ProviderConfigView,
        repo_root: &flotilla_paths::path_context::ExecutionEnvironmentPath,
        runner: Arc<dyn flotilla_provider_runtime::CommandRunner>,
    ) -> Result<Arc<dyn flotilla_vcs::vcs::Vcs>, Vec<flotilla_discovery_api::UnmetRequirement>> {
        if env.find_remote_host("forge.example").is_none() {
            return Err(vec![flotilla_discovery_api::UnmetRequirement::MissingRemoteHost("forge.example".into())]);
        }
        self.0.probe(env, config, repo_root, runner).await
    }
}

// A path not yet cloned has no remote to detect: discovery takes the remote from the
// Clone recorded for that path on this host, and from nothing else.
#[tokio::test]
async fn checkout_vcs_discovery_names_the_repository_a_clone_is_for() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"clone-remote-test\"\n").expect("daemon config");
    let cloned = temp.path().join("cloned");
    let unrecorded = temp.path().join("unrecorded");
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = [&cloned, &unrecorded]
        .into_iter()
        .map(|path| Box::new(RemoteClaiming(FakeVcsFactory::new(FakeVcsState::builder(path).build()))) as Box<_>)
        .collect();
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("local-host"),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let repository = RepositorySpec::remote("https://forge.example/team/repo").expect("repository");
    daemon
        .resource_backend
        .using::<flotilla_resources::Clone>("flotilla")
        .create(
            &test_meta("clone"),
            &flotilla_resources::CloneSpec {
                repo_ref: repository.key(),
                url: "https://forge.example/team/repo".into(),
                env_ref: flotilla_resources::host_direct_environment_name(daemon.environment_manager.local_host_id().as_str()),
                path: cloned.to_string_lossy().into(),
            },
        )
        .await
        .expect("clone");
    assert!(daemon.local_vcs_for_checkout(&cloned).await.is_ok());
    let error = daemon.local_vcs_for_checkout(&unrecorded).await.err().expect("no remote");
    assert!(error.contains("forge.example"), "{error}");
}

#[tokio::test]
async fn repository_watch_evicts_deleted_providers_and_relist_preserves_live_providers() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"provider-watch-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let repositories = backend.clone().using::<Repository>("flotilla");
    let first = RepositorySpec::remote("https://github.com/example/first").expect("first repository");
    let second = RepositorySpec::remote("https://github.com/example/second").expect("second repository");
    for repository in [&first, &second] {
        repositories.create(&test_meta(&repository.key().to_string()), repository).await.expect("create repository");
    }
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let provider: Arc<dyn ChangeRequestTracker> = Arc::new(FakeChangeRequest::new());
    for repository in [&first, &second] {
        daemon.admission.repository_change_requests().write().await.insert(
            repository.key(),
            RepositoryChangeRequestProvider {
                service_url: "https://github.com".to_string(),
                repository: repository.key().to_string(),
                provider: Arc::clone(&provider),
            },
        );
    }
    repositories.delete(&first.key().to_string()).await.expect("delete first repository");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !daemon.admission.repository_change_requests().read().await.contains_key(&first.key()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repository watch should evict provider");
    assert!(daemon.admission.repository_change_requests().read().await.contains_key(&second.key()), "live repository keeps its provider");

    let third = RepositorySpec::remote("https://github.com/example/third").expect("third repository");
    repositories.create(&test_meta(&third.key().to_string()), &third).await.expect("create third repository");
    daemon.admission.repository_change_requests().write().await.insert(
        third.key(),
        RepositoryChangeRequestProvider { service_url: "https://github.com".to_string(), repository: third.key().to_string(), provider },
    );
    backend
        .clone()
        .using::<Repository>("alternate")
        .create(&test_meta(&second.key().to_string()), &second)
        .await
        .expect("repository in replacement namespace");
    daemon.set_provisioning_namespace("alternate".to_string()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !daemon.admission.repository_change_requests().read().await.contains_key(&third.key()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restarted watch should reconcile cached providers from the new list");
    assert!(daemon.admission.repository_change_requests().read().await.contains_key(&second.key()), "relist retains the live provider");
}
