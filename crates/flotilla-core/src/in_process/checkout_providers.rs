//! Observed Checkout lifetimes own discovered VCS capabilities. Active commands
//! retain leases; presentation rows retain descriptors, never provider instances.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::EnvironmentId;
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, Repository, RepositoryKey, ResourceBackend,
    ResourceError, ResourceObject, WatchEvent, WatchStart,
};
use futures::StreamExt;
use tokio::sync::{watch, OnceCell};
use tracing::warn;

use super::{checkout_path, discover_vcs_for_checkout, InProcessDaemon};
use crate::{
    config::ConfigStore,
    environment_manager::ManagedEnvironmentKind,
    path_context::ExecutionEnvironmentPath,
    providers::{discovery::ProviderDescriptor, registry::ProviderRegistry},
    vcs::Vcs,
};

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
    environment_manager: Arc<crate::environment_manager::EnvironmentManager>,
    local_environment_id: EnvironmentId,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
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
        let observed = checkouts.iter().find(|checkout| {
            checkout_path(checkout).is_some_and(|candidate| Path::new(candidate) == path)
                && self.checkout_environment(checkout).as_ref() == Some(env_id)
        });
        let Some(observed) = observed else {
            return discover_vcs_for_checkout(
                &self.environment_manager,
                &self.discovery,
                &self.config,
                &self.local_environment_id,
                env_id,
                path,
            )
            .await
            .map(Arc::new);
        };
        let key = self.checkout_lifetime_key(&namespace, observed).expect("eligible Checkout");
        let cell = {
            let mut cache = self.checkout_vcs.lock().await;
            self.retain_checkout_providers(&mut cache, &namespace, &checkouts);
            cache.entry(key).or_insert_with(|| Arc::new(OnceCell::new())).clone()
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
            discover_vcs_for_checkout(&self.environment_manager, &self.discovery, &scoped_config, &self.local_environment_id, env_id, path)
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
        // Validate the resource instance synchronously: watch delivery may lag
        // behind deletion/recreation. Durable resources still take precedence.
        let checkout = match self.resource_backend.including_replicas::<ResourceCheckout>(namespace).get(&key.name).await {
            Ok(source) => source.object,
            Err(ResourceError::NotFound { .. }) => {
                match self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace).get(&key.name).await {
                    Ok(checkout) => checkout,
                    Err(ResourceError::NotFound { .. }) => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
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

    async fn checkout_provider_facts(&self, namespace: &str) -> Result<Vec<ResourceObject<ResourceCheckout>>, String> {
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
        // Durable resources (including replicas) override ephemeral observations by name.
        for source in
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?.items
        {
            checkouts.insert(source.object.metadata.name.clone(), source.object);
        }
        Ok(checkouts
            .into_values()
            .filter(|checkout| {
                checkout.metadata.deletion_timestamp.is_none()
                    && checkout.status.as_ref().is_none_or(|status| status.phase == CheckoutPhase::Ready)
            })
            .collect())
    }

    pub(super) fn checkout_lifetime_key(
        &self,
        namespace: &str,
        checkout: &ResourceObject<ResourceCheckout>,
    ) -> Option<CheckoutLifetimeKey> {
        if checkout.metadata.deletion_timestamp.is_some()
            || checkout.status.as_ref().is_some_and(|status| status.phase != CheckoutPhase::Ready)
        {
            return None;
        }
        Some(CheckoutLifetimeKey {
            namespace: namespace.into(),
            environment: self.checkout_environment(checkout)?,
            name: checkout.metadata.name.clone(),
            created_at: checkout.metadata.creation_timestamp,
            path: PathBuf::from(checkout_path(checkout)?),
            repository: checkout.spec.repo_ref().clone(),
        })
    }

    fn retain_checkout_providers(&self, cache: &mut CheckoutVcsCache, namespace: &str, checkouts: &[ResourceObject<ResourceCheckout>]) {
        let live: HashSet<_> = checkouts.iter().filter_map(|checkout| self.checkout_lifetime_key(namespace, checkout)).collect();
        cache.retain(|key, _| live.contains(key));
    }

    pub(super) async fn retire_checkout_providers(&self) -> Result<(), String> {
        let namespace = self.provisioning_namespace().await;
        let checkouts = self.checkout_provider_facts(&namespace).await?;
        self.retain_checkout_providers(&mut *self.checkout_vcs.lock().await, &namespace, &checkouts);
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
        for backend in [self.resource_backend.clone(), self.observed_resource_backend.clone()] {
            tokio::spawn(Self::watch_checkout_provider_lifetimes(
                Arc::downgrade(self),
                backend,
                self.checkout_namespace_changes.subscribe(),
            ));
        }
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
