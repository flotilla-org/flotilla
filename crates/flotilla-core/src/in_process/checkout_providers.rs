//! Observed Checkout lifetimes own discovered VCS capabilities. Active commands
//! retain leases; presentation rows retain descriptors, never provider instances.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use flotilla_config::ConfigStore;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_protocol::EnvironmentId;
use flotilla_resources::checkout_path::checkout_path;
use flotilla_resources::checkout_records::{latest_launches, Launches};
use flotilla_resources::discovered_checkouts::DiscoveredCheckouts;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, Repository, RepositoryKey, ResourceObject,
};
use flotilla_store::ResourceBackend;
use tokio::sync::OnceCell;
use tracing::warn;

use super::{discover_vcs_for_checkout, InProcessDaemon};
use flotilla_discovery::environment_manager::ManagedEnvironmentKind;
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_vcs::vcs::Vcs;

pub(super) struct CheckoutProvider {
    pub(super) descriptor: ProviderDescriptor,
    pub(super) vcs: Arc<dyn Vcs>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CheckoutLifetimeKey {
    namespace: String,
    environment: EnvironmentId,
    name: String,
    created_at: DateTime<Utc>,
    path: PathBuf,
    repository: RepositoryKey,
}

pub(super) type CheckoutVcsCache = HashMap<CheckoutLifetimeKey, Arc<OnceCell<Arc<CheckoutProvider>>>>;

/// Shares VCS lookup state between the daemon and CrewService.
/// InProcessDaemon drives retirement as the discovered checkouts change;
/// CrewService uses lookup leases for archive pushes.
#[derive(bon::Builder)]
pub(super) struct CheckoutProviders {
    resource_backend: ResourceBackend,
    /// The checkouts this host's refresh discovered.
    discovered_checkouts: DiscoveredCheckouts,
    config: Arc<ConfigStore>,
    discovery: Arc<super::DiscoveryRuntime>,
    environment_manager: Arc<flotilla_discovery::environment_manager::EnvironmentManager>,
    local_environment_id: EnvironmentId,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    lifecycle_reads: Arc<dyn LifecycleReads>,
    #[builder(default)]
    checkout_vcs: tokio::sync::Mutex<CheckoutVcsCache>,
}

impl CheckoutProviders {
    /// Observed Checkouts own cached providers. Before observation, inspection
    /// uses a transient discovered provider that is never retained by the daemon.
    pub(super) async fn checkout_provider(&self, env_id: &EnvironmentId, path: &Path) -> Result<Arc<CheckoutProvider>, String> {
        let namespace = self.provisioning_namespace().await;
        if let Some(provider) = self.cached_checkout_provider(&namespace, env_id, path).await? {
            return Ok(provider);
        }
        let checkouts = self.checkout_provider_facts(&namespace).await?;
        let observed = checkouts.iter().find(|(_, key)| key.path == path && key.environment == *env_id);
        let Some((observed, key)) = observed else {
            let clone_url = self.clone_url(&namespace, env_id, path).await?;
            return discover_vcs_for_checkout(
                &self.environment_manager,
                &self.discovery,
                &self.config,
                &self.local_environment_id,
                env_id,
                path,
                clone_url.as_deref(),
            )
            .await
            .map(Arc::new);
        };
        let cell = {
            let mut cache = self.checkout_vcs.lock().await;
            Self::retain_checkout_providers(&mut cache, &checkouts);
            cache.entry(key.clone()).or_insert_with(|| Arc::new(OnceCell::new())).clone()
        };
        // A hit has already been validated against the current Checkout instance.
        // Avoid fetching Repository settings and repeating the inventory read.
        if let Some(provider) = cell.get() {
            return Ok(provider.clone());
        }
        let discover = || async {
            // Isolate Repository settings from identical paths in other environments.
            let scoped_config = ConfigStore::with_base(self.config.base_path().as_path());
            let repository = self
                .resource_backend
                .including_replicas::<Repository>(&namespace)
                .get(&observed.spec.repo_ref().to_string())
                .await
                .map_err(|error| error.to_string())?
                .object;
            scoped_config.set_checkout_config(&ExecutionEnvironmentPath::new(path), repository.spec.vcs().clone());
            discover_vcs_for_checkout(
                &self.environment_manager,
                &self.discovery,
                &scoped_config,
                &self.local_environment_id,
                env_id,
                path,
                repository.spec.remotes().first().map(String::as_str),
            )
            .await
            .map(Arc::new)
        };
        let provider = cell.get_or_try_init(discover).await.map(Arc::clone)?;
        // Discovery may have awaited I/O while the Checkout was retired. Keep
        // only current instances, including delete/recreate at the same path.
        // Retirement is part of lease admission. A successful probe cannot prove
        // that a cached resource instance remains current if inventory reads fail;
        // propagate that failure rather than masking it behind a usable provider.
        self.retire_checkout_providers().await?;
        Ok(provider)
    }

    /// The URL of this host's clone at `path` in `env_id`, if one is recorded: the engine's (#3108).
    async fn clone_url(&self, _namespace: &str, env_id: &EnvironmentId, path: &Path) -> Result<Option<String>, String> {
        let clones = self.lifecycle_reads.clones()?.here()?;
        Ok(clones
            .into_iter()
            .find(|clone| {
                Path::new(&clone.spec.path) == path
                    && self
                        .environment_manager
                        .resolve_environment_ref(&clone.spec.env_ref)
                        .is_some_and(|environment| environment.id == *env_id)
            })
            .map(|clone| clone.spec.url))
    }

    async fn cached_checkout_provider(
        &self,
        namespace: &str,
        env_id: &EnvironmentId,
        path: &Path,
    ) -> Result<Option<Arc<CheckoutProvider>>, String> {
        let candidate = self.checkout_vcs.lock().await.iter().find_map(|(key, cell)| {
            if key.namespace == namespace && key.environment == *env_id && key.path == path {
                cell.get().map(|provider| (key.clone(), provider.clone()))
            } else {
                None
            }
        });
        let Some((key, provider)) = candidate else {
            return Ok(None);
        };
        // Validate the checkout instance synchronously: change delivery may lag behind its
        // release and a new launch. The engine's checkouts here take precedence.
        let checkout = match self.lifecycle_reads.checkouts()?.launches(Launches::Named { name: &key.name, home: None })?.pop() {
            Some(record) => record.read_view(),
            None => match self.discovered_checkouts.view(namespace, &key.name) {
                Some(checkout) => checkout,
                None => return Ok(None),
            },
        };
        Ok((self.checkout_lifetime_key(namespace, &checkout).as_ref() == Some(&key)).then_some(provider))
    }

    fn checkout_environment(&self, checkout: &ResourceObject<ResourceCheckout>) -> Option<EnvironmentId> {
        match &checkout.spec {
            ResourceCheckoutSpec::Observed(spec) if spec.host_ref == self.environment_manager.local_host_id().as_str() => {
                Some(self.local_environment_id.clone())
            }
            ResourceCheckoutSpec::Observed(spec) => {
                self.environment_manager.managed_environments().into_iter().find_map(|(id, state)| match state {
                    ManagedEnvironmentKind::Direct(state) if state.host_id.as_ref().is_some_and(|host| host.as_str() == spec.host_ref) => {
                        Some(id)
                    }
                    _ => None,
                })
            }
            spec => spec
                .env_ref()
                .and_then(|reference| self.environment_manager.resolve_environment_ref(reference))
                .map(|environment| environment.id),
        }
    }

    async fn checkout_provider_facts(
        &self,
        namespace: &str,
    ) -> Result<Vec<(ResourceObject<ResourceCheckout>, CheckoutLifetimeKey)>, String> {
        let mut checkouts = self
            .discovered_checkouts
            .views(namespace)
            .into_iter()
            .map(|checkout| (checkout.metadata.name.clone(), checkout))
            .collect::<BTreeMap<_, _>>();
        // The engine's checkouts here override the discovered ones by name (#3108).
        for (name, record) in latest_launches(self.lifecycle_reads.checkouts()?.launches(Launches::Here)?) {
            checkouts.insert(name, record.read_view());
        }
        Ok(checkouts
            .into_values()
            .filter_map(|checkout| self.checkout_lifetime_key(namespace, &checkout).map(|key| (checkout, key)))
            .collect())
    }

    /// The lifetime a checkout's cached provider belongs to: a ready checkout at its path in an
    /// environment of this host, as that instance.
    pub(super) fn checkout_lifetime_key(
        &self,
        namespace: &str,
        checkout: &ResourceObject<ResourceCheckout>,
    ) -> Option<CheckoutLifetimeKey> {
        if checkout.metadata.deletion_timestamp.is_some() {
            return None;
        }
        if checkout.status.as_ref().is_some_and(|status| status.phase != CheckoutPhase::Ready) {
            return None;
        }
        let path =
            checkout.status.as_ref().and_then(|status| status.path.clone()).or_else(|| checkout_path(checkout).map(str::to_owned))?;
        self.checkout_environment(checkout).map(|environment| CheckoutLifetimeKey {
            namespace: namespace.into(),
            environment,
            name: checkout.metadata.name.clone(),
            created_at: checkout.metadata.creation_timestamp,
            path: PathBuf::from(path),
            repository: checkout.spec.repo_ref().clone(),
        })
    }

    fn retain_checkout_providers(cache: &mut CheckoutVcsCache, checkouts: &[(ResourceObject<ResourceCheckout>, CheckoutLifetimeKey)]) {
        let live: HashSet<_> = checkouts.iter().map(|(_, key)| key).collect();
        cache.retain(|key, _| live.contains(key));
    }

    pub(super) async fn retire_checkout_providers(&self) -> Result<(), String> {
        let namespace = self.provisioning_namespace().await;
        let checkouts = self.checkout_provider_facts(&namespace).await?;
        Self::retain_checkout_providers(&mut *self.checkout_vcs.lock().await, &checkouts);
        Ok(())
    }

    /// Resolve capabilities through discovery once for each observed Checkout.
    pub async fn vcs_for_checkout(&self, env_id: &EnvironmentId, checkout: &Path) -> Result<Arc<dyn Vcs>, String> {
        self.checkout_provider(env_id, checkout).await.map(|provider| Arc::clone(&provider.vcs))
    }

    async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }
}

/// Crew resolves checkout VCS through the shared lookup state, defaulting to the local environment.
#[async_trait::async_trait]
impl flotilla_vcs::vcs::CheckoutVcsResolver for CheckoutProviders {
    async fn vcs_for(&self, environment: Option<&EnvironmentId>, path: &Path) -> Result<Arc<dyn Vcs>, String> {
        self.vcs_for_checkout(environment.unwrap_or(&self.local_environment_id), path).await
    }

    async fn vcs_for_env_ref(&self, env_ref: &str, path: &Path) -> Result<Arc<dyn Vcs>, String> {
        let environment = self
            .environment_manager
            .resolve_environment_ref(env_ref)
            .ok_or_else(|| format!("checkout environment {env_ref} is unavailable on this host"))?;
        self.vcs_for_checkout(&environment.id, path).await
    }
}

impl InProcessDaemon {
    pub(super) async fn checkout_provider(&self, env_id: &EnvironmentId, path: &Path) -> Result<Arc<CheckoutProvider>, String> {
        self.checkout_providers.checkout_provider(env_id, path).await
    }

    pub(super) async fn retire_checkout_providers(&self) -> Result<(), String> {
        self.checkout_providers.retire_checkout_providers().await
    }

    pub async fn vcs_for_checkout(&self, env_id: &EnvironmentId, checkout: &Path) -> Result<Arc<dyn Vcs>, String> {
        self.checkout_providers.vcs_for_checkout(env_id, checkout).await
    }

    pub(super) async fn execution_registry(
        &self,
        repository: &ResourceObject<Repository>,
        path: &Path,
    ) -> Result<Arc<ProviderRegistry>, String> {
        let lease = self.repository_providers(repository).await?;
        let mut registry = (*lease.registry).clone();
        let checkout = self.checkout_provider(&self.local_environment_id, path).await?;
        registry.vcs.insert(checkout.descriptor.implementation.clone(), checkout.descriptor.clone(), checkout.vcs.clone());
        Ok(Arc::new(registry))
    }

    /// Retires cached checkout providers as the discovered checkouts, the namespace or the
    /// engine's checkouts change (#3108): a released, lost or newly launched checkout ends the
    /// lifetime its provider belonged to. Started when the engine's readings are installed, since
    /// every retirement reads them; one pass at the start covers changes before then.
    pub(super) fn follow_checkout_provider_lifetimes(&self) -> Result<(), String> {
        let mut engine_changes = self.lifecycle_reads.checkouts()?.changes();
        let mut discovered = self.discovered_checkouts.changes();
        let mut namespace_changes = self.checkout_namespace_changes.subscribe();
        let weak = self.self_weak.clone();
        tokio::spawn(async move {
            loop {
                let Some(daemon) = weak.upgrade() else { return };
                if let Err(error) = daemon.retire_checkout_providers().await {
                    warn!(%error, "retire checkout providers failed");
                }
                drop(daemon);
                tokio::select! {
                    change = engine_changes.recv() => {
                        if let Err(tokio::sync::broadcast::error::RecvError::Closed) = change { return; }
                    }
                    change = discovered.recv() => {
                        if let Err(tokio::sync::broadcast::error::RecvError::Closed) = change { return; }
                    }
                    changed = namespace_changes.changed() => {
                        if changed.is_err() { return; }
                    }
                }
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod lifecycle_contract {
    use super::*;
    use crate::in_process::tests::{discover_checkout, test_engine};
    use crate::testkits::discovery::fake_discovery;
    use flotilla_provisioning_testkit::{CheckoutFixture, Ending};
    use flotilla_resources::CheckoutWorktreeSpec;
    use flotilla_store_memory::InMemoryBackend;

    // Provider facts and lifetime keys follow the engine's checkouts here: none while requested,
    // the ready path once ready, none once lost, and a failed read is an error.
    #[tokio::test]
    async fn provider_lifetimes_follow_engine_checkouts() {
        let temp = tempfile::tempdir().expect("config");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"provider-contract\"\n").expect("config");
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            flotilla_protocol::HostName::new("local"),
            backend.clone(),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let engine = CheckoutFixture::default();
        daemon.install_lifecycle_reads(engine.reads.clone()).expect("install");
        let launch = engine.request(
            "checkout",
            &ResourceCheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: RepositoryKey("github.com/org/repo".into()),
                env_ref: daemon.local_environment_id.to_string(),
                r#ref: "main".into(),
                base_ref: None,
                target_path: "/engine-path".into(),
                clone_ref: "clone".into(),
            }),
            None,
        );
        assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("requested").is_empty());
        engine.finish(&launch, Ending::Ready("/engine-path"));
        let facts = daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("ready");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1.path, PathBuf::from("/engine-path"));
        engine.finish(&launch, Ending::Lost("/engine-path"));
        assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("lost").is_empty());
        engine.poison();
        assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect_err("checked read").contains("poisoned"));
    }

    // Startup: a daemon's composition observes checkouts before its runtime installs the engine's
    // readings. Retirement reads them, so nothing retires before then and no retirement fails;
    // the follower starts with the readings.
    #[tokio::test]
    async fn checkouts_discovered_before_the_engine_readings_fail_no_retirement() {
        #[derive(Clone, Default)]
        struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lines").extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines::default();
        let writer = lines.clone();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish());
        let temp = tempfile::tempdir().expect("config");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"provider-startup\"\n").expect("config");
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            flotilla_protocol::HostName::new("local"),
            ResourceBackend::new(InMemoryBackend::default()),
            Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let observed = |path: &str| {
            flotilla_resources::ObservedCheckoutSpec::builder()
                .r#ref("main".into())
                .path(path.into())
                .repo_ref(RepositoryKey("github.com/org/repo".into()))
                .host_ref(daemon.environment_manager.local_host_id().to_string())
                .is_main(true)
                .build()
        };
        discover_checkout(&daemon, "early", observed("/early"));
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        test_engine(&daemon);
        discover_checkout(&daemon, "later", observed("/later"));
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        let logged = String::from_utf8(lines.0.lock().expect("lines").clone()).expect("utf8");
        assert!(!logged.contains("retire checkout providers failed"), "{logged}");
    }
}
