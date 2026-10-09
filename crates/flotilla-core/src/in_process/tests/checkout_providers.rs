use std::sync::Arc;
use std::time::Duration;

use flotilla_protocol::HostName;
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, InMemoryBackend,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec, Repository, RepositorySpec, ResourceBackend,
};

use super::support::test_meta;
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::RepositoryChangeRequestProvider;
use crate::in_process::InProcessDaemon;
use crate::providers::change_request::ChangeRequestTracker;
use crate::testkits::discovery::{fake_discovery, FakeChangeRequest, FakeVcsFactory, FakeVcsState};

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
    let daemon =
        InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("local-host")).await;

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
#[tokio::test]
async fn repository_watch_evicts_deleted_providers_and_relist_preserves_live_providers() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"provider-watch-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let provider: Arc<dyn ChangeRequestTracker> = Arc::new(FakeChangeRequest::new());
    for repository in [&first, &second] {
        daemon.convoy_admission.repository_change_requests.write().await.insert(
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
            if !daemon.convoy_admission.repository_change_requests.read().await.contains_key(&first.key()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repository watch should evict provider");
    assert!(
        daemon.convoy_admission.repository_change_requests.read().await.contains_key(&second.key()),
        "live repository keeps its provider"
    );

    let third = RepositorySpec::remote("https://github.com/example/third").expect("third repository");
    repositories.create(&test_meta(&third.key().to_string()), &third).await.expect("create third repository");
    daemon.convoy_admission.repository_change_requests.write().await.insert(
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
            if !daemon.convoy_admission.repository_change_requests.read().await.contains_key(&third.key()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restarted watch should reconcile cached providers from the new list");
    assert!(
        daemon.convoy_admission.repository_change_requests.read().await.contains_key(&second.key()),
        "relist retains the live provider"
    );
}
