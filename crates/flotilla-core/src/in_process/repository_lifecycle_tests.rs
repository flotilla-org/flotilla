//! #1770: identity and provider lifetimes follow resources, not presentation roots.

use std::sync::atomic::{AtomicUsize, Ordering};

use hegel::generators as gs;

use super::*;
use crate::providers::{
    discovery::{
        detectors::generic::{parse_first_dotted_version, CommandDetector},
        factories::github::GitHubIssueProviderFactory,
        test_support::{
            fake_discovery, fake_discovery_with_runner, FakeIssueProvider, FakePresentationManager, FakePresentationManagerFactory,
            FakeVcsFactory, FakeVcsState,
        },
        Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement,
    },
    testing::MockRunner,
};

struct CountingVcsFactory {
    inner: FakeVcsFactory,
    probes: Arc<AtomicUsize>,
}

#[async_trait]
impl Factory for CountingVcsFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn crate::vcs::Vcs;

    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    async fn probe(
        &self,
        bag: &EnvironmentBag,
        config: &ConfigStore,
        path: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn crate::vcs::Vcs>, Vec<UnmetRequirement>> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        // Real discovery awaits subprocess I/O. Keep this fake probe in flight
        // so concurrent callers exercise OnceCell initialization, not just hits.
        tokio::task::yield_now().await;
        self.inner.probe(bag, config, path, runner).await
    }
}

async fn observe(daemon: &InProcessDaemon, backend: &ResourceBackend, name: &str, path: &Path, key: &RepositoryKey) {
    backend
        .clone()
        .using::<ResourceCheckout>(&daemon.provisioning_namespace().await)
        .create(
            &InputMeta::builder().name(name.to_string()).build(),
            &ResourceCheckoutSpec::Observed(
                flotilla_resources::ObservedCheckoutSpec::builder()
                    .r#ref("main".into())
                    .path(path.to_string_lossy().into_owned())
                    .repo_ref(key.clone())
                    .host_ref(daemon.environment_manager.local_host_id().to_string())
                    .is_main(true)
                    .build(),
            ),
        )
        .await
        .expect("observe checkout");
}

// #1770: observation alone never constructs a provider. Concurrent demands
// share one discovered provider, deletion releases it, and recreation gets a
// new provider even when the resource name and path are reused.
// Generator spans both backends, one to three lifetimes, duplicate requests,
// both deletion orders, and an empty inventory between lifetimes.
#[hegel::test]
fn observed_checkout_provider_lifetimes(tc: hegel::TestCase) {
    let durable = tc.draw(gs::booleans());
    let cycles = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let duplicates = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let reverse = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("config dir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"checkout-lifetime\"\n").expect("daemon config");
        let path = Path::new("/checkouts/lifetime");
        let probes = Arc::new(AtomicUsize::new(0));
        let mut discovery = fake_discovery(false);
        discovery.factories.vcs =
            vec![Box::new(CountingVcsFactory { inner: FakeVcsFactory::new(FakeVcsState::builder(path).build()), probes: probes.clone() })];
        let daemon =
            InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("test")).await;
        let backend = if durable { &daemon.resource_backend } else { &daemon.observed_resource_backend };
        let repository = RepositorySpec::remote("https://github.com/example/lifetime").expect("repository");
        let key = repository.key();
        daemon
            .resource_backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &repository)
            .await
            .expect("create repository");
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        for cycle in 0..cycles {
            observe(&daemon, backend, "lifetime", path, &key).await;
            assert_eq!(probes.load(Ordering::SeqCst), cycle * 2, "observation is not provider demand");
            let (first, concurrent) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(daemon.local_vcs_for_checkout(path), daemon.local_vcs_for_checkout(path))
            })
            .await
            .expect("concurrent discovery must complete without deadlock");
            let first = first.expect("first demand");
            assert!(Arc::ptr_eq(&first, &concurrent.expect("concurrent demand")));
            for _ in 0..duplicates {
                assert!(Arc::ptr_eq(&first, &daemon.local_vcs_for_checkout(path).await.expect("duplicate demand")));
            }
            assert_eq!(probes.load(Ordering::SeqCst), cycle * 2 + 1, "one construction for this Checkout lifetime");
            let old = Arc::downgrade(&first);
            backend.clone().using::<ResourceCheckout>("flotilla").delete("lifetime").await.expect("delete checkout");
            // Recreate before waiting for a watcher: instance identity must also
            // prevent stale reuse across a fast delete/create pair.
            observe(&daemon, backend, "lifetime", path, &key).await;
            let replacement = daemon.local_vcs_for_checkout(path).await.expect("replacement demand");
            assert!(!Arc::ptr_eq(&first, &replacement));
            assert_eq!(probes.load(Ordering::SeqCst), cycle * 2 + 2);
            if reverse {
                drop(replacement);
                drop(first);
            } else {
                drop(first);
                drop(replacement);
            }
            assert!(old.upgrade().is_none(), "retired instance must not be retained");
            let live = daemon.local_vcs_for_checkout(path).await.expect("live replacement");
            let retired = Arc::downgrade(&live);
            drop(live);
            backend.clone().using::<ResourceCheckout>("flotilla").delete("lifetime").await.expect("retire replacement");
            tokio::time::timeout(Duration::from_secs(2), async {
                while retired.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("Checkout watch retires provider without another demand");
        }
    });
}

// #1770: generic checkout commands execute using a Repository key and observed
// Checkout even when the daemon has no legacy RepoState or observation root.
#[tokio::test]
async fn generic_checkout_executes_without_presentation_roots() {
    let temp = tempfile::tempdir().expect("config dir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"repository-lifecycle-test\"\n").expect("daemon config");
    let path = Path::new("/checkouts/commands");
    let state = FakeVcsState::builder(path).checkout("main").path(path).is_main(true).build().build();
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![Box::new(FakeVcsFactory::new(state.clone()))];
    discovery.factories.presentation_managers = vec![Box::new(FakePresentationManagerFactory(Arc::new(FakePresentationManager::new())))];
    let daemon = InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("test")).await;
    let spec = RepositorySpec::remote("https://github.com/acme/commands").expect("repository");
    let key = spec.key();
    daemon
        .resource_backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("declare Repository");
    observe(&daemon, &daemon.observed_resource_backend, "commands", path, &key).await;
    assert!(daemon.list_repos().await.expect("presentation rows").is_empty());
    let mut events = daemon.subscribe();
    let id = daemon
        .execute(
            Command::builder()
                .action(flotilla_protocol::CommandAction::Checkout {
                    repo: flotilla_protocol::RepoSelector::Repository(key),
                    target: flotilla_protocol::CheckoutTarget::FreshBranch("new-branch".into()),
                    issue_ids: Vec::new(),
                })
                .build(),
        )
        .await
        .expect("checkout admitted by resource identity");
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("command event") {
                if command_id == id {
                    break result;
                }
            }
        }
    })
    .await
    .expect("command completed");
    assert!(matches!(result, CommandValue::CheckoutCreated { ref branch, .. } if branch == "new-branch"), "{result:?}");
    assert!(state.read().expect("fake VCS").checkouts.iter().any(|(_, checkout)| checkout.branch == "new-branch"));
}

// Subprocess/network boundary: host probes use canned responses; only GitHub
// API requests go through the queued conditional-response runner.
struct ConditionalIssueRunner {
    api: MockRunner,
}

#[async_trait]
impl CommandRunner for ConditionalIssueRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "gh" && args.first() == Some(&"api") {
            return self.api.run(cmd, args, cwd, label).await;
        }
        match (cmd, args) {
            ("git", ["--version"]) => Ok("git version 2.43.0".into()),
            ("gh", ["--version"]) => Ok("gh version 2.49.0".into()),
            _ => Err(format!("unexpected command {cmd} {args:?}")),
        }
    }

    async fn exists(&self, cmd: &str, _args: &[&str]) -> bool {
        matches!(cmd, "gh" | "git")
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<crate::providers::CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|stdout| crate::providers::CommandOutput { stdout, stderr: String::new(), success: true })
    }
}

// Operator's second ruling: source/Project binding selection must retain its
// provider lease and conditional-request cache across checkout removal. A stale
// presentation registry must not redirect source selection to another provider.
#[tokio::test]
async fn project_issue_binding_keeps_conditional_lease_after_checkout_removal() {
    let temp = tempfile::tempdir().expect("config dir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"repository-lifecycle-test\"\n").expect("daemon config");
    let runner = Arc::new(ConditionalIssueRunner { api: MockRunner::new(vec![
        Ok("HTTP/2 200 OK\r\nETag: \"issue-window\"\r\nX-RateLimit-Remaining: 4800\r\n\r\n[{\"number\":1,\"title\":\"Changed\",\"state\":\"open\",\"labels\":[],\"updated_at\":\"2026-07-01T00:00:10Z\"}]".into()),
        Ok("HTTP/2 304 Not Modified\r\nETag: \"issue-window\"\r\nX-RateLimit-Remaining: 4800\r\n\r\n".into()),
    ]) });
    let mut discovery = fake_discovery_with_runner(false, runner.clone());
    discovery.host_detectors.push(Box::new(CommandDetector::new("gh", &["--version"], parse_first_dotted_version)));
    discovery.factories.issue_trackers = vec![Box::new(GitHubIssueProviderFactory)];
    let daemon = InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("test")).await;
    let spec = RepositorySpec::remote("https://github.com/acme/issues").expect("repository");
    let key = spec.key();
    daemon
        .resource_backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("Repository");
    let source = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "acme/issues".into() };
    daemon
        .resource_backend
        .clone()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("issues".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Issues".into())
                .default_workflow_ref("single-agent".into())
                .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder().repo(key.clone()).build()])
                .issue_source_bindings(vec![source.clone().into()])
                .build(),
        )
        .await
        .expect("Project binding");
    let path = Path::new("/checkouts/issues");
    observe(&daemon, &daemon.observed_resource_backend, "issues", path, &key).await;
    let identity = repository_operations::repository_event_identity(&spec, None);
    let mut stale = ProviderRegistry::new();
    stale.issue_trackers.insert(
        "stale",
        ProviderDescriptor::named(ProviderCategory::IssueProvider, "stale"),
        Arc::new(FakeIssueProvider::new()),
    );
    daemon.repos.write().await.insert(
        identity.clone(),
        RepoState::new(identity, RepoRootState {
            path: path.into(),
            model: RepoModel::new(stale, None),
            slug: None,
            unmet: Vec::new(),
            is_local: true,
        }),
    );
    let first = daemon.issue_provider_for_source(&source).await.expect("source provider");
    let initial = first.list_changed_since(&source, "2026-07-01T00:00:00Z", 50).await.expect("initial poll");
    assert_eq!(initial.updated.len(), 1);
    daemon.remove_repo(path).await.expect("remove checkout observation");
    let second = daemon.issue_provider_for_source(&source).await.expect("retained source provider");
    assert!(Arc::ptr_eq(&first, &second), "an observer's strong lease survives checkout removal");
    let changed = second.list_changed_since(&source, "2026-07-01T00:00:20Z", 50).await.expect("conditional poll");
    assert!(changed.updated.is_empty() && changed.closed.is_empty());
    let calls = runner.api.calls();
    assert_eq!(calls.len(), 2, "one request per poll; no reload or extra quota use");
    assert_eq!(calls[0].1[2], calls[1].1[2], "stable conditional endpoint");
    assert!(calls[1].1.contains(&"If-None-Match: \"issue-window\"".to_string()));
}

// Namespace changes retire idle providers immediately and move both resource
// watchers to the new namespace; deletion there still needs no subsequent demand.
#[tokio::test]
async fn checkout_provider_watch_follows_namespace_changes() {
    let temp = tempfile::tempdir().expect("config");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"namespace-lifetime\"\n").expect("config");
    let path = Path::new("/checkouts/namespace");
    let mut discovery = fake_discovery(false);
    discovery.factories.vcs = vec![Box::new(FakeVcsFactory::new(FakeVcsState::builder(path).build()))];
    let daemon = InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), discovery, HostName::new("test")).await;
    let spec = RepositorySpec::remote("https://github.com/example/namespace").expect("repository");
    let key = spec.key();
    for namespace in ["flotilla", "other"] {
        daemon
            .resource_backend
            .using::<Repository>(namespace)
            .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
            .await
            .expect("repository");
    }
    observe(&daemon, &daemon.observed_resource_backend, "namespace", path, &key).await;
    let first = daemon.local_vcs_for_checkout(path).await.expect("first provider");
    let first_lifetime = Arc::downgrade(&first);
    // Let both in-memory watch subscriptions reach their pending receive before
    // switching; this exercises notification rather than initial inventory.
    tokio::task::yield_now().await;
    drop(first);
    daemon.set_provisioning_namespace("other".into()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while first_lifetime.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("namespace change retires the previous cached lifetime without demand");
    for backend in [&daemon.resource_backend, &daemon.observed_resource_backend] {
        observe(&daemon, backend, "namespace", path, &key).await;
        let provider = daemon.local_vcs_for_checkout(path).await.expect("new namespace provider");
        let retired = Arc::downgrade(&provider);
        drop(provider);
        backend.using::<ResourceCheckout>("other").delete("namespace").await.expect("delete checkout");
        tokio::time::timeout(Duration::from_secs(2), async {
            while retired.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("watch in the new namespace retires the provider without demand");
    }
}

// #1722 bindings select an issue source independently of the live checkout
// transport. A canonical Forgejo source must stay usable after a Github mirror
// becomes the live transport, without a checkout or presentation row.
#[tokio::test]
async fn project_issue_binding_uses_source_forge_instead_of_mirror_transport() {
    use flotilla_resources::{Forge, ForgeKind, ForgeSpec};

    use crate::providers::discovery::factories::github::ForgejoIssueProviderFactory;

    let temp = tempfile::tempdir().expect("config");
    let config_dir = temp.path().join("config");
    std::fs::create_dir(&config_dir).expect("config dir");
    std::fs::write(config_dir.join("daemon.toml"), "machine_id = \"source-forge\"\n").expect("config");
    std::fs::write(temp.path().join("lab-forgejo-source-token"), "test-token\n").expect("credential");
    let mut discovery = fake_discovery(false);
    discovery.factories.issue_trackers = vec![Box::new(ForgejoIssueProviderFactory)];
    let daemon = InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(config_dir)), discovery, HostName::new("test")).await;
    let forge = ForgeSpec::builder()
        .forge_id("source-forge".into())
        .kind(ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forge.example".into()]))
        .https_url("https://forge.example".into())
        .git_ssh_host("forge.example".into())
        .build();
    daemon
        .resource_backend
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("source-forge".into()).build(), &forge)
        .await
        .expect("Forge");
    let spec = RepositorySpec::remote("https://forge.example/acme/issues")
        .expect("canonical source")
        .update_remotes("https://github.com/acme/issues")
        .expect("mirror transport");
    let source = flotilla_protocol::IssueSource { service: "https://forge.example".into(), scope: "acme/issues".into() };
    assert_eq!(spec.forge().expect("live forge").service_url, "https://github.com");
    assert_eq!(spec.issue_source_forge().expect("issue source").service_url, source.service);
    daemon
        .resource_backend
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(spec.key().to_string()).build(), &spec)
        .await
        .expect("Repository");
    daemon
        .resource_backend
        .definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("issues".into()).build(),
            &ProjectSpec::builder()
                .display_name("Issues".into())
                .default_workflow_ref("single-agent".into())
                .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder().repo(spec.key()).build()])
                .issue_source_bindings(vec![source.clone().into()])
                .build(),
        )
        .await
        .expect("Project binding");
    let first = daemon.issue_provider_for_source(&source).await.expect("source-owned Forgejo capability");
    assert!(first.supports(&source));
    let second = daemon.issue_provider_for_source(&source).await.expect("retained source capability");
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(
        daemon.resource_backend.using::<Repository>("flotilla").get(&spec.key().to_string()).await.expect("stored Repository").spec,
        spec,
        "source selection must not rewrite checkout transport intent"
    );
    assert!(daemon.list_repos().await.expect("presentation rows").is_empty());
}
