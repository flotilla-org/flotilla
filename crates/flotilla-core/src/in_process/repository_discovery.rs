//! Repository-scoped provider discovery supplied by core.
use super::*;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_forge_forgejo::FORGEJO_AUTH_PROVIDER;
use flotilla_vcs::providers::detection::remote_assertion;
pub(super) async fn discover_repository_change_request_with(
    resource_backend: &ResourceBackend,
    config: &ConfigStore,
    discovery: &DiscoveryRuntime,
    environment_manager: &EnvironmentManager,
    local_environment_id: &EnvironmentId,
    namespace: &str,
    repository: &RepositorySpec,
) -> Result<Arc<dyn ChangeRequestTracker>, String> {
    let identity = repository.forge().ok_or("no forge identity")?;
    let bag = repository_provider_bag(resource_backend, config, environment_manager, local_environment_id, namespace, repository).await?;
    discovery.observation_credentials.require_for_bag(discovery.runner.as_ref(), &bag).await?;
    let probe_root = ExecutionEnvironmentPath::new(config.base_path().as_ref());
    let mut unmet = Vec::new();
    for factory in &discovery.factories.change_requests {
        match factory.probe(&bag, config, &probe_root, Arc::clone(&discovery.runner)).await {
            Ok(provider) => {
                return Ok(Arc::new(flotilla_observation::forge_observation::ObservedChangeRequestTracker {
                    inner: provider,
                    reads: flotilla_observation::forge_observation::ForgeReads::new(resource_backend.clone(), namespace.into()),
                    source: flotilla_protocol::IssueSource { service: identity.service_url.clone(), scope: identity.repository.clone() },
                }))
            }
            Err(requirements) => unmet.extend(requirements.into_iter().map(|requirement| format!("{requirement:?}"))),
        }
    }
    Err(format!("change request provider unavailable for {} ({})", identity.service_url, unmet.join(", ")))
}

pub(super) async fn repository_provider_bag(
    resource_backend: &ResourceBackend,
    config: &ConfigStore,
    environment_manager: &EnvironmentManager,
    local_environment_id: &EnvironmentId,
    namespace: &str,
    repository: &RepositorySpec,
) -> Result<EnvironmentBag, String> {
    let host_bag = environment_manager.environment_bag(local_environment_id).ok_or("local discovery environment unavailable")?;
    // Host capabilities cannot contribute an ambient checkout's forge target.
    let host_bag = host_bag
        .assertions()
        .iter()
        .filter(|assertion| {
            !matches!(
                assertion,
                EnvironmentAssertion::AuthFileExists { provider, .. } if provider == FORGEJO_AUTH_PROVIDER
            ) && !matches!(
                assertion,
                EnvironmentAssertion::RemoteHost { .. }
                    | EnvironmentAssertion::OriginForge { .. }
                    | EnvironmentAssertion::ForgeOverride { .. }
                    | EnvironmentAssertion::VcsCheckoutDetected { .. }
            )
        })
        .fold(EnvironmentBag::new(), |bag, assertion| bag.with(assertion.clone()));
    let Some(identity) = repository.forge() else {
        return Ok(host_bag);
    };
    let remote = format!("{}/{}", identity.service_url.trim_end_matches('/'), identity.repository);
    let forge = match repository.identity() {
        RepositoryIdentity::Forge { forge_ref, .. } => Some(
            resource_backend
                .including_replicas::<Forge>(namespace)
                .get(forge_ref)
                .await
                .map_err(|error| format!("Forge {forge_ref}: {error}"))?
                .object
                .spec,
        ),
        _ => forge_for_remote(resource_backend, namespace, &remote).await?,
    };
    let remote_assertion = remote_assertion(&remote, "origin").ok_or_else(|| format!("invalid repository remote {remote}"))?;
    let mut bag = host_bag.with(remote_assertion);
    if let Some(forge) = &forge {
        bag = bag.with(EnvironmentAssertion::origin_forge(forge.clone()));
    }
    // Resolve execution independently of the logical Forge, including builtin GitHub.
    let daemon = config.load_daemon_config()?;
    if let Some(binding) = daemon.forge_overrides.get(identity.service_url.trim_end_matches('/')) {
        binding.validate()?;
        return Ok(bag.with(EnvironmentAssertion::ForgeOverride { binding: binding.clone() }));
    }
    if let Some(forge) = &forge {
        if forge.kind == ForgeKind::Forgejo {
            // Read current intent on each request: provider leases compare the
            // resolved bag so a changed identity cannot reuse a stale credential.
            if let Some(name) = daemon.credentials.forgejo.get(&forge.forge_id) {
                let credential = resource_backend
                    .definitions::<CredentialSpec>(namespace)
                    .get(name)
                    .await
                    .map_err(|error| format!("daemon Forgejo credential {name}: {error}"))?;
                match (&credential.spec.consumer, &credential.spec.source) {
                    (CredentialConsumer::Forgejo { forge_ref, .. }, CredentialSource::File { path }) if forge_ref == &forge.forge_id => {
                        bag = bag.with(EnvironmentAssertion::auth_file(FORGEJO_AUTH_PROVIDER, path));
                    }
                    _ => {
                        return Err(format!("daemon Forgejo credential {name} must use a file source and target Forge {}", forge.forge_id))
                    }
                }
            }
        }
    }
    Ok(bag)
}
