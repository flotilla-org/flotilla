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
use flotilla_protocol::{EnvironmentId, LifecycleReading};
use flotilla_resources::checkout_path::checkout_path;
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads, ProvisioningKind};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, ReadResourceObject, Repository, RepositoryKey,
    ResourceError, ResourceObject, ResourceProvenance, WatchEvent, WatchStart,
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
                .get(&observed.object.spec.repo_ref().to_string())
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
            Ok(source) => source,
            Err(ResourceError::NotFound { .. }) => {
                match self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace).get(&key.name).await {
                    Ok(checkout) => ReadResourceObject { object: checkout, provenance: ResourceProvenance::Local },
                    Err(ResourceError::NotFound { .. }) => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok((self.checked_checkout_lifetime_key(namespace, &checkout)?.as_ref() == Some(&key)).then_some(provider))
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
    ) -> Result<Vec<(ReadResourceObject<ResourceCheckout>, CheckoutLifetimeKey)>, String> {
        let mut checkouts = self
            .observed_resource_backend
            .clone()
            .using::<ResourceCheckout>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|checkout| {
                (checkout.metadata.name.clone(), ReadResourceObject { object: checkout, provenance: ResourceProvenance::Local })
            })
            .collect::<BTreeMap<_, _>>();
        // Durable resources (including replicas) override ephemeral observations by name.
        for source in
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?.items
        {
            checkouts.insert(source.object.metadata.name.clone(), source);
        }
        checkouts
            .into_values()
            .filter_map(|source| match self.checked_checkout_lifetime_key(namespace, &source) {
                Ok(Some(key)) => Some(Ok((source, key))),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub(super) fn checkout_lifetime_key(
        &self,
        namespace: &str,
        checkout: &ResourceObject<ResourceCheckout>,
    ) -> Option<CheckoutLifetimeKey> {
        // Watch keys are invalidation hints. Admission below propagates read failures rather
        // than accepting a provider using stale lifecycle evidence.
        self.checked_checkout_lifetime_key(
            namespace,
            &ReadResourceObject { object: checkout.clone(), provenance: ResourceProvenance::Local },
        )
        .unwrap_or_else(|error| {
            warn!(%error, "checkout lifetime read failed");
            None
        })
    }

    fn checked_checkout_lifetime_key(
        &self,
        namespace: &str,
        source: &ReadResourceObject<ResourceCheckout>,
    ) -> Result<Option<CheckoutLifetimeKey>, String> {
        let checkout = &source.object;
        if checkout.metadata.deletion_timestamp.is_some() {
            return Ok(None);
        }
        let origin = match &source.provenance {
            ResourceProvenance::Local => None,
            ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
        };
        let lifecycle = if matches!(checkout.spec, ResourceCheckoutSpec::Observed(_)) {
            None
        } else {
            self.lifecycle_reads.try_reading(ProvisioningKind::Checkout, &checkout.metadata.name, origin)?
        };
        let path = match lifecycle {
            Some(LifecycleReading::Ready { identity }) => identity,
            Some(_) => return Ok(None),
            None => {
                if checkout.status.as_ref().is_some_and(|status| status.phase != CheckoutPhase::Ready) {
                    return Ok(None);
                }
                let Some(path) = checkout_path(checkout) else { return Ok(None) };
                path.to_string()
            }
        };
        Ok(self.checkout_environment(checkout).map(|environment| CheckoutLifetimeKey {
            namespace: namespace.into(),
            environment,
            name: checkout.metadata.name.clone(),
            created_at: checkout.metadata.creation_timestamp,
            path: PathBuf::from(path),
            repository: checkout.spec.repo_ref().clone(),
        }))
    }

    fn retain_checkout_providers(cache: &mut CheckoutVcsCache, checkouts: &[(ReadResourceObject<ResourceCheckout>, CheckoutLifetimeKey)]) {
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

#[cfg(test)]
mod lifecycle_contract {
    use super::*;
    use crate::testkits::discovery::fake_discovery;
    use flotilla_protocol::NodeId;
    use flotilla_provisioning::engine::checkout::CheckoutBinding;
    use flotilla_provisioning_testkit::{Ending, LifecycleFixture};
    use flotilla_resources::{CheckoutStatus, CheckoutWorktreeSpec, InputMeta, CONVOY_LABEL};
    use flotilla_store_memory::InMemoryBackend;

    // Provider facts and lifetime keys use the same checked engine readiness/path at each origin.
    #[tokio::test]
    async fn provider_lifetimes_follow_engine_without_a_copy() {
        for (replica, race) in [(false, false), (true, false), (false, true)] {
            let temp = tempfile::tempdir().expect("config");
            std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"provider-contract\"\n").expect("config");
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let author = if replica { ResourceBackend::new(InMemoryBackend::default()) } else { backend.clone() };
            let daemon = InProcessDaemon::new_with_resource_backend(
                Vec::new(),
                Arc::new(ConfigStore::with_base(temp.path())),
                fake_discovery(false),
                flotilla_protocol::HostName::new("local"),
                backend.clone(),
                Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
            )
            .await;
            let records = author.using::<ResourceCheckout>("flotilla");
            let mut obj = records
                .create(
                    &InputMeta::builder().name("checkout".into()).labels([(CONVOY_LABEL.into(), "crew".into())].into()).build(),
                    &ResourceCheckoutSpec::Worktree(CheckoutWorktreeSpec {
                        repo_ref: RepositoryKey("github.com/org/repo".into()),
                        env_ref: daemon.local_environment_id.to_string(),
                        r#ref: "main".into(),
                        base_ref: None,
                        target_path: "/work".into(),
                        clone_ref: "clone".into(),
                    }),
                )
                .await
                .expect("checkout");
            obj = records
                .update_status(
                    "checkout",
                    &obj.metadata.resource_version,
                    &CheckoutStatus { phase: CheckoutPhase::Gone, path: Some("/stale".into()), ..Default::default() },
                )
                .await
                .expect("stale status");
            if replica {
                backend
                    .replica_writer::<ResourceCheckout>(NodeId::new("home"), "flotilla")
                    .replace(&records.list().await.expect("snapshot"), Utc::now())
                    .await
                    .expect("replica");
            }
            let fixture = LifecycleFixture::<CheckoutBinding>::new(replica);
            if race {
                // Live read boundary: two Ready observations, then concurrent backing loss.
                struct ReadyThenLost(std::sync::atomic::AtomicUsize);
                impl LifecycleReads for ReadyThenLost {
                    fn try_reading(&self, _: ProvisioningKind, _: &str, _: Option<&str>) -> Result<Option<LifecycleReading>, String> {
                        use std::sync::atomic::Ordering;
                        Ok(Some(if self.0.fetch_add(1, Ordering::SeqCst) < 2 {
                            LifecycleReading::Ready { identity: "/engine-path".into() }
                        } else {
                            LifecycleReading::Lost
                        }))
                    }
                }
                let reads = Arc::new(ReadyThenLost(std::sync::atomic::AtomicUsize::new(0)));
                daemon.install_lifecycle_reads(reads.clone()).expect("install");
                // Repository lookup fails normally; eligibility changes must never panic acquisition.
                assert!(daemon
                    .checkout_providers
                    .checkout_provider(&daemon.local_environment_id, Path::new("/engine-path"))
                    .await
                    .is_err());
                assert_eq!(reads.0.load(std::sync::atomic::Ordering::SeqCst), 1, "one checked read per inventory entry");
                assert!(daemon
                    .checkout_providers
                    .checkout_provider(&daemon.local_environment_id, Path::new("/engine-path"))
                    .await
                    .is_err());
                assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("lost").is_empty());
                continue;
            }
            daemon.install_lifecycle_reads(fixture.reads.clone()).expect("install");
            fixture.request(&obj);
            assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("requested").is_empty());
            fixture.finish(&obj, Ending::Ready("/engine-path"));
            let facts = daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("ready");
            assert_eq!(facts.len(), 1);
            let key = &facts[0].1;
            assert_eq!(key.path, PathBuf::from("/engine-path"));
            fixture.finish(&obj, Ending::Lost("/engine-path"));
            assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect("lost").is_empty());
            fixture.poison();
            assert!(daemon.checkout_providers.checkout_provider_facts("flotilla").await.expect_err("checked read").contains("poisoned"));
        }
    }
}
