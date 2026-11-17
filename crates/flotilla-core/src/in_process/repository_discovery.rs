//! Repository-scoped provider discovery supplied by core.
use super::*;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_forge_forgejo::FORGEJO_AUTH_PROVIDER;
use flotilla_observation::forge_observation::ForgeReads;
use flotilla_vcs::{
    forge_credentials::{GitHttpCredential, GitHttpCredentials},
    providers::detection::remote_assertion,
};
pub(super) async fn discover_repository_change_request_with(
    forge_reads: &ForgeReads,
    config: &ConfigStore,
    discovery: &DiscoveryRuntime,
    environment_manager: &EnvironmentManager,
    local_environment_id: &EnvironmentId,
    namespace: &str,
    repository: &RepositorySpec,
) -> Result<Arc<dyn ChangeRequestTracker>, String> {
    let identity = repository.forge().ok_or("no forge identity")?;
    let bag =
        repository_provider_bag(&forge_reads.backend, config, environment_manager, local_environment_id, namespace, repository).await?;
    discovery.observation_credentials.require_for_bag(discovery.runner.as_ref(), &bag).await?;
    let probe_root = ExecutionEnvironmentPath::new(config.base_path().as_ref());
    let mut unmet = Vec::new();
    for factory in &discovery.factories.change_requests {
        match factory.probe(&bag, config, &probe_root, Arc::clone(&discovery.runner)).await {
            Ok(provider) => {
                return Ok(Arc::new(flotilla_observation::forge_observation::ObservedChangeRequestTracker {
                    inner: provider,
                    reads: {
                        let mut reads = forge_reads.clone();
                        reads.namespace = namespace.into();
                        reads
                    },
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
        // Read current intent on each request: provider leases compare the
        // resolved bag so a changed identity cannot reuse a stale credential.
        if let Some(identity) = daemon_forgejo_identity(resource_backend, &daemon, namespace, forge).await? {
            bag = bag.with(EnvironmentAssertion::auth_file(FORGEJO_AUTH_PROVIDER, &identity.token_file));
        }
    }
    Ok(bag)
}

/// The daemon's own identity on a Forgejo `forge`: the file-sourced credential `[credentials.forgejo]`
/// maps the forge to. GitHub forges and unmapped Forgejo forges have none.
async fn daemon_forgejo_identity(
    resource_backend: &ResourceBackend,
    daemon: &flotilla_config::DaemonConfig,
    namespace: &str,
    forge: &flotilla_resources::ForgeSpec,
) -> Result<Option<GitHttpCredential>, String> {
    if forge.kind != ForgeKind::Forgejo {
        return Ok(None);
    }
    let Some(name) = daemon.credentials.forgejo.get(&forge.forge_id) else {
        return Ok(None);
    };
    let credential = resource_backend
        .definitions::<CredentialSpec>(namespace)
        .get(name)
        .await
        .map_err(|error| format!("daemon Forgejo credential {name}: {error}"))?;
    match (&credential.spec.consumer, &credential.spec.source) {
        (CredentialConsumer::Forgejo { forge_ref, username }, CredentialSource::File { path }) if forge_ref == &forge.forge_id => {
            let host = forge
                .https_url
                .strip_prefix("https://")
                .and_then(|front| front.split('/').next())
                .ok_or_else(|| format!("Forge {} https_url names no host", forge.forge_id))?;
            Ok(Some(GitHttpCredential { host: host.to_string(), username: username.clone(), token_file: path.clone() }))
        }
        _ => Err(format!("daemon Forgejo credential {name} must use a file source and target Forge {}", forge.forge_id)),
    }
}

/// The daemon's identities on the forges `[credentials.forgejo]` maps, which every Git
/// invocation of the local environment's checkout providers carries. A mapping whose Forge is
/// not declared yet contributes nothing; host health reports it.
pub(super) struct DaemonForgeCredentials {
    pub(super) resource_backend: ResourceBackend,
    pub(super) config: Arc<ConfigStore>,
    pub(super) provisioning_namespace: Arc<std::sync::RwLock<String>>,
}

#[async_trait::async_trait]
impl GitHttpCredentials for DaemonForgeCredentials {
    async fn current(&self) -> Result<Vec<GitHttpCredential>, String> {
        let daemon = self.config.load_daemon_config()?;
        if daemon.credentials.forgejo.is_empty() {
            return Ok(Vec::new());
        }
        let namespace = self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone();
        let mut identities = Vec::new();
        for forge_id in daemon.credentials.forgejo.keys() {
            let forge = match self.resource_backend.including_replicas::<Forge>(&namespace).get(forge_id).await {
                Ok(forge) => forge.object.spec,
                Err(ResourceError::NotFound { .. }) => continue,
                Err(error) => return Err(format!("Forge {forge_id}: {error}")),
            };
            identities.extend(daemon_forgejo_identity(&self.resource_backend, &daemon, &namespace, &forge).await?);
        }
        Ok(identities)
    }
}
