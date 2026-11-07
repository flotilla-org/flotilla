//! Observed Checkout lifetimes own discovered VCS capabilities. Active commands
//! retain leases; presentation rows retain descriptors, never provider instances.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_config::ConfigStore;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_protocol::EnvironmentId;
use flotilla_resources::checkout_path::checkout_path;
use flotilla_resources::checkout_records::latest_launches;
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, Repository, RepositoryKey, ResourceError,
    ResourceObject, WatchEvent, WatchStart,
};
use flotilla_store::ResourceBackend;
use futures::StreamExt;
use tokio::sync::{watch, OnceCell};
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
/// InProcessDaemon drives retirement through the observed-checkout watch;
/// CrewService uses lookup leases for archive pushes.
#[derive(bon::Builder)]
pub(super) struct CheckoutProviders {
    resource_backend: ResourceBackend,
    observed_resource_backend: ResourceBackend,
    config: Arc<ConfigStore>,
    discovery: Arc<super::DiscoveryRuntime>,
    environment_manager: Arc<flotilla_discovery::environment_manager::EnvironmentManager>,
    local_environment_id: EnvironmentId,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    #[builder(default = Arc::new(NoLifecycleReads))]
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
        let checkout = match self.lifecycle_reads.checkouts()?.get(&key.name, None)? {
            Some(record) => record.read_view(),
            None => match self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace).get(&key.name).await {
                Ok(checkout) => checkout,
                Err(ResourceError::NotFound { .. }) => return Ok(None),
                Err(error) => return Err(error.to_string()),
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
            .observed_resource_backend
            .clone()
            .using::<ResourceCheckout>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|checkout| (checkout.metadata.name.clone(), checkout))
            .collect::<BTreeMap<_, _>>();
        // The engine's checkouts here override ephemeral observations by name (#3108).
        for (name, record) in latest_launches(self.lifecycle_reads.checkouts()?.here()?) {
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

    fn checkout_lifetime_key(&self, namespace: &str, checkout: &ResourceObject<ResourceCheckout>) -> Option<CheckoutLifetimeKey> {
        self.checkout_providers.checkout_lifetime_key(namespace, checkout)
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

    pub(super) fn spawn_checkout_provider_retirement(self: &Arc<Self>) {
        tokio::spawn(Self::watch_checkout_provider_lifetimes(
            Arc::downgrade(self),
            self.observed_resource_backend.clone(),
            self.checkout_namespace_changes.subscribe(),
        ));
    }

    /// Retires cached checkout providers as the engine's checkouts change (#3108): a released,
    /// lost or newly launched checkout ends the lifetime its provider belonged to. Started by
    /// composition once the engine's readings are installed.
    pub fn follow_checkout_provider_lifetimes(self: &Arc<Self>) -> Result<(), String> {
        let mut changes = self.lifecycle_reads.checkouts()?.changes();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
                let Some(daemon) = weak.upgrade() else { return };
                if let Err(error) = daemon.retire_checkout_providers().await {
                    warn!(%error, "retire checkout providers failed");
                }
            }
        });
        Ok(())
    }

    async fn watch_checkout_provider_lifetimes(weak: Weak<Self>, backend: ResourceBackend, mut namespace_changes: watch::Receiver<()>) {
        while let Some(daemon) = weak.upgrade() {
            // Mark the current notification before listing; changes during setup
            // stay pending and immediately restart the subscription below.
            namespace_changes.borrow_and_update();
            let namespace = daemon.provisioning_namespace().await;
            let checkouts = backend.clone().using::<ResourceCheckout>(&namespace);
            let setup = async {
                let listed = checkouts.list().await?;
                let watch = checkouts.watch(WatchStart::resuming_from(&listed)).await?;
                Ok::<_, ResourceError>((listed, watch))
            }
            .await;
            let (listed, mut watch) = match setup {
                Ok(setup) => setup,
                Err(error) => {
                    warn!(%error, "subscribe to checkout provider lifetimes failed");
                    drop(daemon);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let mut known: HashMap<_, _> = listed
                .items
                .iter()
                .map(|checkout| (checkout.metadata.name.clone(), daemon.checkout_lifetime_key(&namespace, checkout)))
                .collect();
            if let Err(error) = daemon.retire_checkout_providers().await {
                warn!(%error, "retire checkout providers failed");
            }
            drop(daemon);
            let mut retry = false;
            loop {
                let event = tokio::select! {
                    event = watch.next() => match event {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => { warn!(%error, "checkout provider lifetime watch failed"); retry = true; break; },
                        None => { retry = true; break; },
                    },
                    changed = namespace_changes.changed() => {
                        if changed.is_err() { return; }
                        break;
                    }
                };
                let Some(daemon) = weak.upgrade() else {
                    return;
                };
                let changed = match event {
                    WatchEvent::Added(checkout) | WatchEvent::Modified(checkout) => {
                        let key = daemon.checkout_lifetime_key(&namespace, &checkout);
                        known.insert(checkout.metadata.name, key.clone()) != Some(key)
                    }
                    WatchEvent::Deleted(checkout) => {
                        known.remove(&checkout.metadata.name);
                        true
                    }
                    WatchEvent::DeletedByName(tombstone) => {
                        known.remove(&tombstone.name);
                        true
                    }
                };
                // Status-only changes that leave the eligible instance unchanged
                // cannot alter any cached capability and need no full inventory.
                if changed {
                    if let Err(error) = daemon.retire_checkout_providers().await {
                        warn!(%error, "retire checkout providers failed");
                    }
                }
            }
            if retry {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(test)]
mod lifecycle_contract {
    use super::*;
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
}
