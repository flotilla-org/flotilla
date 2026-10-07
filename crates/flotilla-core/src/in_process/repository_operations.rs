//! Repository capabilities are independent of observation roots and working copies.
use super::*;
use crate::{
    in_process::convoy_admission::repository_provider_bag,
    providers::{
        discovery::{Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        registry::ProviderSet,
    },
};

/// Forge identity or Repository key identifies a row independently of any
/// execution location, including repositories that have no forge.
pub(super) fn repository_event_identity(spec: &RepositorySpec, _path: Option<&Path>) -> RepoIdentity {
    if let Some(forge) = spec.forge() {
        let authority = forge
            .service_url
            .strip_prefix("https://")
            .or_else(|| forge.service_url.strip_prefix("http://"))
            .unwrap_or(&forge.service_url)
            .trim_end_matches('/')
            .to_string();
        RepoIdentity { authority, path: forge.repository.clone() }
    } else {
        RepoIdentity { authority: "repository".into(), path: spec.key().to_string() }
    }
}

#[derive(bon::Builder)]
pub(super) struct RepositoryProviderLease {
    spec: RepositorySpec,
    bag: EnvironmentBag,
    config: serde_json::Value,
    runner: Arc<dyn CommandRunner>,
    pub(super) registry: Arc<ProviderRegistry>,
    unmet: Vec<(String, UnmetRequirement)>,
}

async fn probe<T: ?Sized + Send + Sync + 'static>(
    factories: &[Box<dyn Factory<Descriptor = ProviderDescriptor, Output = T>>],
    bag: &EnvironmentBag,
    config: &ConfigStore,
    runner: &Arc<dyn CommandRunner>,
    root: &ExecutionEnvironmentPath,
    providers: &mut ProviderSet<T>,
    unmet: &mut Vec<(String, UnmetRequirement)>,
) {
    for factory in factories {
        let desc = factory.descriptor();
        match factory.probe(bag, config, root, runner.clone()).await {
            Ok(provider) => providers.insert(desc.implementation.clone(), desc, provider),
            Err(requirements) => unmet.extend(requirements.into_iter().map(|requirement| (desc.implementation.clone(), requirement))),
        }
    }
}

impl InProcessDaemon {
    pub(super) async fn repository_for_selector(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<ResourceObject<Repository>, String> {
        let key = self.resolve_repository_selector(selector).await?.ok_or_else(|| format!("no Repository matches '{selector}'"))?;
        self.resource_backend
            .including_replicas::<Repository>(&self.provisioning_namespace().await)
            .get(&key.to_string())
            .await
            .map(|source| source.object)
            .map_err(|error| error.to_string())
    }

    pub(super) async fn local_checkout_for_repository(&self, key: &RepositoryKey) -> Result<Option<PathBuf>, String> {
        let checkouts = crate::repository_addressing::local_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            &self.provisioning_namespace().await,
            self.environment_manager.local_host_id().as_str(),
        )
        .await?;
        let mut paths = checkouts
            .into_iter()
            .filter(|checkout| checkout.spec.repo_ref() == key)
            .filter_map(|checkout| match checkout.spec {
                ResourceCheckoutSpec::Observed(spec) => Some((!spec.is_main, PathBuf::from(spec.path))),
                _ => checkout.status.and_then(|status| status.path).map(|path| (true, PathBuf::from(path))),
            })
            .collect::<Vec<_>>();
        // Observed main checkouts rank first; other Ready checkouts use lexical
        // order rather than the retiring tracked-root preferred_path projection.
        paths.sort();
        Ok(paths.into_iter().next().map(|(_, path)| path))
    }

    pub(super) async fn repository_providers(
        &self,
        repository: &ResourceObject<Repository>,
    ) -> Result<Arc<RepositoryProviderLease>, String> {
        let namespace = self.provisioning_namespace().await;
        let bag = repository_provider_bag(
            &self.resource_backend,
            &self.config,
            &self.environment_manager,
            &self.local_environment_id,
            &namespace,
            &repository.spec,
        )
        .await?;
        let runner = self.environment_manager.environment_runner(&self.local_environment_id).ok_or("local runner unavailable")?;
        let host_config = self.config.load_config();
        let config = serde_json::to_value(&host_config).map_err(|error| error.to_string())?;
        let key = (namespace, repository.spec.key());
        let cache = self.repository_providers.lock().await;
        if let Some(lease) = cache.get(&key) {
            if lease.spec == repository.spec
                && lease.bag.assertions() == bag.assertions()
                && lease.config == config
                && Arc::ptr_eq(&lease.runner, &runner)
            {
                return Ok(Arc::clone(lease));
            }
        }
        drop(cache);
        // Probes may perform I/O; never serialize unrelated Repository lookups.
        let mut registry = ProviderRegistry::new();
        let mut unmet = Vec::new();
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_path());
        probe(
            &self.discovery.factories.change_requests,
            &bag,
            &self.config,
            &runner,
            &probe_root,
            &mut registry.change_requests,
            &mut unmet,
        )
        .await;
        probe(&self.discovery.factories.issue_trackers, &bag, &self.config, &runner, &probe_root, &mut registry.issue_trackers, &mut unmet)
            .await;
        let default_backend = host_config.change_request.preference.backend;
        if let Some(backend) = repository.spec.change_request().backend.as_deref().or(default_backend.as_deref()) {
            if !registry.change_requests.prefer_by_backend(backend) {
                unmet.push((
                    "change_request".into(),
                    UnmetRequirement::UnknownProviderPreference { category: ProviderCategory::ChangeRequest, key: backend.into() },
                ));
            }
        }
        if let Some(backend) = host_config.issue_tracker.preference.backend {
            if !registry.issue_trackers.prefer_by_backend(&backend) {
                unmet.push((
                    "issue_tracker".into(),
                    UnmetRequirement::UnknownProviderPreference { category: ProviderCategory::IssueProvider, key: backend },
                ));
            }
        }
        let host_bag = self.environment_manager.local_environment_bag();
        let host = self
            .discovery
            .host_scoped_providers
            .discover_for_environment(
                &self.local_environment_id,
                &host_bag,
                &self.discovery.factories,
                &self.config,
                &probe_root,
                runner.clone(),
            )
            .await;
        host.install(&mut registry, &mut unmet);
        let lease = Arc::new(
            RepositoryProviderLease::builder()
                .spec(repository.spec.clone())
                .bag(bag)
                .config(config)
                .runner(runner)
                .registry(Arc::new(registry))
                .unmet(unmet)
                .build(),
        );
        // Concurrent misses may probe twice; last writer wins, and every later
        // lookup validates its inputs before reuse. Commands retain their lease.
        self.repository_providers.lock().await.insert(key, Arc::clone(&lease));
        Ok(lease)
    }

    pub(super) async fn execute_action_repository_forge(&self, command_id: u64, command: &Command) -> Result<u64, String> {
        let selector = command.context_repo.as_ref().ok_or("command requires Repository context")?;
        let repository = self.repository_for_selector(selector).await?;
        if repository.spec.forge().is_none() {
            return Err("Repository has no forge identity".into());
        }
        let identity = repository_event_identity(&repository.spec, None);
        let lease = self.repository_providers(&repository).await?;
        let action = command.action.clone();
        let event_sink = self.event_sink.clone();
        let node_id = self.node_id.clone();
        let active_commands = Arc::clone(&self.active_commands);
        let cancel = CancellationToken::new();
        active_commands.lock().await.insert(command_id, cancel.clone());
        event_sink.emit(DaemonEvent::CommandStarted {
            command_id,
            node_id: node_id.clone(),
            repo_identity: identity.clone(),
            repo: None,
            description: command.description().into(),
        });
        tokio::spawn(async move {
            let operation = async {
                if let CommandAction::OpenIssue { id } = &action {
                    let forge = repository.spec.issue_source_forge().ok_or("Repository has no forge issue source")?;
                    let source = flotilla_protocol::IssueSource { service: forge.service_url, scope: forge.repository };
                    let provider = lease.registry.issue_provider_for(&source).ok_or("no issue provider available for Repository")?;
                    return provider.open_in_browser(&flotilla_protocol::IssueRef { source, id: id.clone() }).await;
                }
                if let CommandAction::MergeChangeRequest { id, confirmed } = &action {
                    if repository.spec.is_fork() {
                        return Err(format!("merging change request {id} is forbidden for fork-stance repository; landing is human-only"));
                    }
                    if !confirmed {
                        return Err(format!("merging change request {id} requires explicit confirmation"));
                    }
                }
                let provider =
                    lease.registry.change_requests.preferred().ok_or("no change request provider is active for this Repository")?;
                match action {
                    CommandAction::OpenChangeRequest { id } => provider.open_in_browser(&id).await,
                    CommandAction::CloseChangeRequest { id } => provider.close_change_request(&id).await,
                    CommandAction::MergeChangeRequest { id, .. } => provider.merge_change_request(&id).await,
                    CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
                        provider.link_issues(&change_request_id, &issue_ids).await
                    }
                    _ => Err("not a Repository forge action".into()),
                }
            };
            let result = tokio::select! {
                result = operation => match result { Ok(()) => CommandValue::Ok, Err(message) => CommandValue::Error { message } },
                _ = cancel.cancelled() => CommandValue::Cancelled,
            };
            active_commands.lock().await.remove(&command_id);
            event_sink.emit(DaemonEvent::CommandFinished { command_id, node_id, repo_identity: identity, repo: None, result });
        });
        Ok(command_id)
    }

    pub(super) async fn repository_providers_response(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<RepoProvidersResponse, String> {
        let repository = self.repository_for_selector(selector).await?;
        let lease = self.repository_providers(&repository).await?;
        let path = self.local_checkout_for_repository(&repository.spec.key()).await?;
        let mut providers = lease
            .registry
            .provider_infos()
            .into_iter()
            .map(|(category, name)| ProviderInfo { category, name, healthy: true, disabled_reason: None })
            .collect::<Vec<_>>();
        let mut checkout_bag = self.environment_manager.local_environment_bag();
        let mut checkout_unmet = Vec::new();
        if let Some(path) = &path {
            let root = ExecutionEnvironmentPath::new(path);
            for detector in &self.discovery.repo_detectors {
                checkout_bag = checkout_bag.extend(detector.detect(&root, lease.runner.as_ref(), self.discovery.env.as_ref()).await);
            }
            match self.checkout_provider(&self.local_environment_id, path).await {
                Ok(checkout) => providers.push(ProviderInfo {
                    category: checkout.descriptor.category.slug().into(),
                    name: checkout.descriptor.display_name.clone(),
                    healthy: true,
                    disabled_reason: None,
                }),
                Err(error) => {
                    for factory in &self.discovery.factories.vcs {
                        let descriptor = factory.descriptor();
                        providers.push(ProviderInfo {
                            category: descriptor.category.slug().into(),
                            name: descriptor.display_name.clone(),
                            healthy: false,
                            disabled_reason: Some(error.clone()),
                        });
                        checkout_unmet.push((descriptor.implementation, UnmetRequirement::NoVcsCheckout));
                    }
                }
            }
        }
        let mut repo_discovery = lease
            .bag
            .assertions()
            .iter()
            .filter(|assertion| {
                matches!(
                    assertion,
                    EnvironmentAssertion::RemoteHost { .. }
                        | EnvironmentAssertion::OriginForge { .. }
                        | EnvironmentAssertion::AuthFileExists { .. }
                )
            })
            .map(crate::convert::assertion_to_discovery_entry)
            .collect::<Vec<_>>();
        repo_discovery.extend(
            checkout_bag
                .assertions()
                .iter()
                .filter(|assertion| matches!(assertion, EnvironmentAssertion::VcsCheckoutDetected { .. }))
                .map(crate::convert::assertion_to_discovery_entry),
        );
        Ok(RepoProvidersResponse {
            repository: repository.spec.key(),
            path,
            slug: repository.spec.forge().map(|forge| forge.repository.clone()),
            host_discovery: self
                .environment_manager
                .local_environment_bag()
                .assertions()
                .iter()
                .map(crate::convert::assertion_to_discovery_entry)
                .collect(),
            repo_discovery,
            providers,
            unmet_requirements: lease
                .unmet
                .iter()
                .chain(checkout_unmet.iter())
                .map(|(factory, req)| crate::convert::unmet_requirement_to_proto(factory, req))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Event routing must preserve forge authority without a transport scheme.
    #[test]
    fn event_identity_preserves_http_authority() {
        let spec = RepositorySpec::remote("http://forge.example/team/repo").expect("Repository");
        assert_eq!(repository_event_identity(&spec, None), RepoIdentity { authority: "forge.example".into(), path: "team/repo".into() });
    }
}
