//! Observed Checkout lifetimes own discovered VCS capabilities. Active commands
//! retain leases; presentation rows retain descriptors, never provider instances.

use super::*;

pub(super) struct CheckoutProvider {
    pub(super) descriptor: crate::providers::discovery::ProviderDescriptor,
    pub(super) vcs: Arc<dyn crate::vcs::Vcs>,
}

pub(super) type CheckoutVcsCache =
    HashMap<(String, EnvironmentId, String, DateTime<Utc>, PathBuf), Arc<tokio::sync::OnceCell<Arc<CheckoutProvider>>>>;

impl InProcessDaemon {
    /// Observed Checkouts own cached providers. Before observation, inspection
    /// uses a transient discovered provider that is never retained by the daemon.
    pub(super) async fn checkout_provider(&self, env_id: &EnvironmentId, path: &Path) -> Result<Arc<CheckoutProvider>, String> {
        let namespace = self.provisioning_namespace().await;
        let checkouts = self.checkout_provider_facts(&namespace).await?;
        let observed = checkouts.iter().find(|checkout| {
            checkout_path(checkout).is_some_and(|candidate| Path::new(candidate) == path)
                && self.checkout_environment(checkout).as_ref() == Some(env_id)
        });
        // Settings come from the Checkout's Repository, in an isolated overlay
        // so identical paths in different environments cannot overwrite intent.
        let scoped_config = ConfigStore::with_base(self.config.base_path().as_path());
        if let Some(checkout) = observed {
            let repository = self
                .resource_backend
                .including_replicas::<Repository>(&namespace)
                .get(&checkout.spec.repo_ref().to_string())
                .await
                .map_err(|error| error.to_string())?
                .object;
            scoped_config.set_checkout_config(&ExecutionEnvironmentPath::new(path), repository.spec.vcs().clone());
        }
        let config = if observed.is_some() { &scoped_config } else { &self.config };
        let discover = || async {
            discover_vcs_for_checkout(&self.environment_manager, &self.discovery, config, &self.local_environment_id, env_id, path)
                .await
                .map(Arc::new)
        };
        let Some(observed) = observed else { return discover().await };
        let key =
            (namespace.clone(), env_id.clone(), observed.metadata.name.clone(), observed.metadata.creation_timestamp, path.to_path_buf());
        let cell = {
            let mut cache = self.checkout_vcs.lock().await;
            self.retain_checkout_providers(&mut cache, &namespace, &checkouts);
            cache.entry(key).or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())).clone()
        };
        let provider = cell.get_or_try_init(discover).await.map(Arc::clone)?;
        // Discovery may have awaited I/O while the Checkout was retired. Keep
        // only current instances, including delete/recreate at the same path.
        self.retire_checkout_providers().await?;
        Ok(provider)
    }

    fn checkout_environment(&self, checkout: &ResourceObject<ResourceCheckout>) -> Option<EnvironmentId> {
        match &checkout.spec {
            ResourceCheckoutSpec::Observed(spec) if spec.host_ref == self.environment_manager.local_host_id().as_str() => {
                Some(self.local_environment_id.clone())
            }
            ResourceCheckoutSpec::Observed(spec) => {
                self.environment_manager.managed_environments().into_iter().find_map(|(id, state)| match state {
                    crate::environment_manager::ManagedEnvironmentKind::Direct(state)
                        if state.host_id.as_ref().is_some_and(|host| host.as_str() == spec.host_ref) =>
                    {
                        Some(id)
                    }
                    _ => None,
                })
            }
            spec => spec.env_ref().and_then(|reference| self.resolve_environment_ref(reference)).map(|environment| environment.id),
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
        for source in
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?.items
        {
            checkouts.insert(source.object.metadata.name.clone(), source.object);
        }
        Ok(checkouts
            .into_values()
            .filter(|checkout| {
                checkout.metadata.deletion_timestamp.is_none()
                    && checkout.status.as_ref().is_none_or(|status| status.phase == flotilla_resources::CheckoutPhase::Ready)
            })
            .collect())
    }

    fn retain_checkout_providers(&self, cache: &mut CheckoutVcsCache, namespace: &str, checkouts: &[ResourceObject<ResourceCheckout>]) {
        cache.retain(|(cached_namespace, environment, name, created_at, path), _| {
            cached_namespace == namespace
                && checkouts.iter().any(|checkout| {
                    checkout.metadata.name == *name
                        && checkout.metadata.creation_timestamp == *created_at
                        && self.checkout_environment(checkout).as_ref() == Some(environment)
                        && checkout_path(checkout).is_some_and(|candidate| Path::new(candidate) == path)
                })
        });
    }

    pub(super) async fn retire_checkout_providers(&self) -> Result<(), String> {
        let namespace = self.provisioning_namespace().await;
        let checkouts = self.checkout_provider_facts(&namespace).await?;
        self.retain_checkout_providers(&mut *self.checkout_vcs.lock().await, &namespace, &checkouts);
        Ok(())
    }

    /// Resolve capabilities through discovery once for each observed Checkout.
    pub async fn vcs_for_checkout(&self, env_id: &EnvironmentId, checkout: &Path) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
        self.checkout_provider(env_id, checkout).await.map(|provider| Arc::clone(&provider.vcs))
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
        // Both stores contribute Checkout lifetimes. List/watch handoff ensures
        // deletion cannot be lost between the initial inventory and subscription.
        for backend in [self.resource_backend.clone(), self.observed_resource_backend.clone()] {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                while let Some(daemon) = weak.upgrade() {
                    let namespace = daemon.provisioning_namespace().await;
                    let checkouts = backend.clone().using::<ResourceCheckout>(&namespace);
                    let listed = match checkouts.list().await {
                        Ok(listed) => listed,
                        Err(error) => {
                            warn!(%error, "list checkouts for provider retirement failed");
                            drop(daemon);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };
                    let mut watch = match checkouts.watch(WatchStart::resuming_from(&listed)).await {
                        Ok(watch) => watch,
                        Err(error) => {
                            warn!(%error, "watch checkouts for provider retirement failed");
                            drop(daemon);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };
                    if let Err(error) = daemon.retire_checkout_providers().await {
                        warn!(%error, "retire checkout providers failed");
                    }
                    drop(daemon);
                    let mut namespace_check = tokio::time::interval(Duration::from_secs(1));
                    namespace_check.tick().await;
                    loop {
                        tokio::select! {
                            event = watch.next() => match event {
                                Some(Ok(_)) => {},
                                Some(Err(error)) => { warn!(%error, "checkout provider lifetime watch failed"); break },
                                None => break,
                            },
                            _ = namespace_check.tick() => {
                                let Some(daemon) = weak.upgrade() else { return };
                                if daemon.provisioning_namespace().await != namespace { break }
                                continue;
                            }
                        }
                        let Some(daemon) = weak.upgrade() else { return };
                        if let Err(error) = daemon.retire_checkout_providers().await {
                            warn!(%error, "retire checkout providers failed");
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }
    }
}
