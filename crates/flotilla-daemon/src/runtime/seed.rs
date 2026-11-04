//! Startup resource seeding: this host's own resources and fulfilment kinds.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_core::in_process::InProcessDaemon;

use flotilla_resources::{
    canonical_host_id, definition_provenance::SEEDING_MANAGER, descriptive_repo_slug, is_prepared_snapshot, Clone, CloneSpec, Environment,
    EnvironmentSpec, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, Host, HostDirectEnvironmentSpec, HostSpec,
    InputMeta, PlacementImageSource, Platform, Repository, ResourceError, ResourceObject, MANAGED_BY_LABEL, PLACEMENT_SNAPSHOT_KIND,
};
use flotilla_store::ResourceBackend;
use tracing::warn;

use super::discovery::{LocalProvisioningProfile, DEFAULT_DOCKER_IMAGE};

pub(super) async fn register_startup_resources(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let backend = daemon.resource_backend();
    ensure_host_exists(&backend, namespace, &profile.host_id, &profile.display_name, profile.max_vessels)
        .await
        .map_err(|error| format!("ensure_host_exists: {error}"))?;
    ensure_host_direct_environment_exists(&backend, namespace, profile)
        .await
        .map_err(|error| format!("ensure_host_direct_environment_exists: {error}"))?;
    discover_local_clones(daemon, &backend, namespace, profile).await.map_err(|error| format!("discover_local_clones: {error}"))?;
    ensure_default_kinds(&backend, namespace, profile, std::env::consts::OS)
        .await
        .map_err(|error| format!("ensure_default_kinds: {error}"))?;
    Ok(())
}

pub(super) async fn ensure_host_exists(
    backend: &ResourceBackend,
    namespace: &str,
    host_name: &str,
    display_name: &str,
    max_vessels: Option<u32>,
) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace);
    match hosts.get(host_name).await {
        Ok(existing) if existing.spec.display_name == display_name && existing.spec.max_vessels == max_vessels => return Ok(()),
        Ok(existing) => {
            return hosts
                .update(
                    &InputMeta::from(&existing.metadata),
                    &existing.metadata.resource_version,
                    &HostSpec {
                        display_name: display_name.to_string(),
                        connection: Default::default(),
                        expected_concurrent_rust_crews: existing.spec.expected_concurrent_rust_crews,
                        image_build_capacity: existing.spec.image_build_capacity.clone(),
                        // This path seeds the local daemon Host only, whose cap
                        // bounds its Docker placements and which is never a
                        // single-tenant SSH host box (#4002). SSH hosts,
                        // their cap and tenancy are written by
                        // `register_ssh_host_resources`.
                        max_vessels,
                        single_tenant: false,
                    },
                )
                .await
                .map(|_| ())
                .map_err(|err| err.to_string());
        }
        Err(ResourceError::NotFound { .. }) => {}
        Err(err) => return Err(format!("check host {host_name}: {err}")),
    }
    hosts
        .create(
            &empty_meta(host_name),
            &HostSpec { display_name: display_name.to_string(), connection: Default::default(), max_vessels, ..HostSpec::default() },
        )
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

pub(super) async fn ensure_host_direct_environment_exists(
    backend: &ResourceBackend,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let name = profile.host_direct_environment_name();
    let environments = backend.clone().using::<Environment>(namespace);
    match environments.get(&name).await {
        Ok(_) => return Ok(()),
        Err(ResourceError::NotFound { .. }) => {}
        Err(err) => return Err(format!("check environment {name}: {err}")),
    }

    let address = flotilla_address::Address::root(flotilla_address::Kind::Host, &profile.host_id)
        .and_then(|place| place.child(flotilla_address::Kind::Provider, &profile.host_direct_provider))
        .map_err(|error| error.to_string())?;

    environments
        .create(
            &empty_meta(&name),
            &EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: Some(address),
                host_direct: Some(HostDirectEnvironmentSpec {
                    host_ref: profile.host_id.clone(),
                    repo_default_dir: profile.repo_default_dir.clone(),
                }),
                docker: None,
            },
        )
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

pub(super) async fn discover_local_clones(
    daemon: &Arc<InProcessDaemon>,
    backend: &ResourceBackend,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let clones = backend.clone().using::<Clone>(namespace);
    let host_direct_env_ref = profile.host_direct_environment_name();

    for repo_path in daemon.tracked_repo_paths().await {
        let inspection = match daemon.inspect_repository_path(&repo_path, None).await {
            Ok(inspection) => inspection,
            Err(err) => {
                warn!(path = %repo_path.display(), %err, "skipping clone discovery because repository identity resolution failed");
                continue;
            }
        };
        let Some(transport_url) = inspection.transport_url else {
            continue;
        };
        let canonical_url = match inspection.spec.identity() {
            flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
            flotilla_resources::RepositoryIdentity::Forge { .. } => {
                let forge = inspection.spec.forge().expect("forge Repository has a service");
                format!("{}/{}", forge.service_url, forge.repository)
            }
            flotilla_resources::RepositoryIdentity::Local { .. } => continue,
        };
        let repository_spec = inspection.spec;
        let repository_key = repository_spec.key();
        flotilla_store::ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, &repository_spec)
            .await
            .map_err(|error| error.to_string())?;
        let repo_key_value = repository_key.to_string();
        let name = format!("clone-{}", repository_spec.clone_key(&host_direct_env_ref)?);
        let expected_spec = CloneSpec {
            repo_ref: repository_key.clone(),
            url: if matches!(repository_spec.identity(), flotilla_resources::RepositoryIdentity::Forge { .. }) {
                canonical_url.clone()
            } else {
                transport_url.clone()
            },
            env_ref: host_direct_env_ref.clone(),
            path: repo_path.display().to_string(),
        };
        let expected_labels = BTreeMap::from([
            ("flotilla.work/discovered".to_string(), "true".to_string()),
            ("flotilla.work/repo-key".to_string(), repo_key_value),
            ("flotilla.work/env".to_string(), host_direct_env_ref.clone()),
            ("flotilla.work/repo".to_string(), descriptive_repo_slug(&canonical_url)),
        ]);

        match clones.get(&name).await {
            Ok(existing) => {
                if existing.metadata.deletion_timestamp.is_some() {
                    continue;
                }
                if existing.spec.repo_ref != repository_key || existing.spec.env_ref != host_direct_env_ref {
                    warn!(clone = %name, "leaving discovered clone untouched because the existing resource does not match the expected repo/env tuple");
                    continue;
                }

                let merged_labels = merged_labels(&existing.metadata.labels, &expected_labels);
                if existing.spec != expected_spec || existing.metadata.labels != merged_labels {
                    clones
                        .update(&meta_from_existing(&existing, merged_labels), &existing.metadata.resource_version, &expected_spec)
                        .await
                        .map_err(|err| err.to_string())?;
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                clones.create(&empty_meta_with_labels(&name, expected_labels), &expected_spec).await.map_err(|err| err.to_string())?;
            }
            Err(err) => return Err(err.to_string()),
        }
    }

    Ok(())
}

/// Seed this host's kinds: host-direct always, and Docker when it is available. Seeding owns
/// their topology; grants others refine are kept, and the platform is the host's.
pub(super) async fn ensure_default_kinds(
    backend: &ResourceBackend,
    namespace: &str,
    profile: &LocalProvisioningProfile,
    platform: &str,
) -> Result<(), String> {
    let host_direct = FulfilmentKindSpec {
        host_ref: profile.host_id.clone(),
        pool: profile.host_direct_pool.clone(),
        cost_class: Default::default(),
        grants: BTreeSet::from([
            FulfilmentGrant::platform(platform.to_owned()),
            FulfilmentGrant::host_account_reach(),
            FulfilmentGrant::network("host".to_owned()),
        ]),
        realisation: FulfilmentRealisation::HostDirect,
    };
    seed_kind(backend, namespace, &profile.host_direct_policy_name(), host_direct).await?;
    if profile.docker_available {
        let docker = FulfilmentKindSpec {
            host_ref: profile.host_id.clone(),
            pool: profile.docker_pool.clone(),
            cost_class: Default::default(),
            grants: BTreeSet::from([FulfilmentGrant::platform(Platform::Linux.to_string()), FulfilmentGrant::network("scoped".to_owned())]),
            realisation: FulfilmentRealisation::DockerPerVessel { image: Some(PlacementImageSource::Literal(DEFAULT_DOCKER_IMAGE.into())) },
        };
        seed_kind(backend, namespace, &profile.docker_policy_name(), docker).await?;
    }
    canonicalize_kind_hosts(backend, namespace, &profile.host_id).await
}

async fn seed_kind(backend: &ResourceBackend, namespace: &str, name: &str, desired: FulfilmentKindSpec) -> Result<(), String> {
    let kinds = backend.clone().using::<FulfilmentKind>(namespace);
    let seeded = BTreeMap::from([(MANAGED_BY_LABEL.to_owned(), SEEDING_MANAGER.to_owned())]);
    match kinds.get(name).await {
        Ok(existing) if existing.metadata.deletion_timestamp.is_some() => Ok(()),
        Ok(existing) if existing.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|manager| manager != SEEDING_MANAGER) => {
            warn!(kind = %name, "leaving a managed fulfilment kind untouched during seeding");
            Ok(())
        }
        Ok(existing) => {
            let mut spec = desired.clone();
            spec.cost_class = existing.spec.cost_class;
            spec.grants = existing.spec.grants.iter().filter(|grant| !grant.0.starts_with("platform:")).cloned().collect();
            spec.grants.extend(desired.grants.iter().filter(|grant| grant.0.starts_with("platform:")).cloned());
            if spec == existing.spec && existing.metadata.labels.contains_key(MANAGED_BY_LABEL) {
                return Ok(());
            }
            let labels = merged_labels(&existing.metadata.labels, &seeded);
            kinds
                .update(&meta_from_existing(&existing, labels), &existing.metadata.resource_version, &spec)
                .await
                .map(|_| ())
                .map_err(|error| format!("seed fulfilment kind {name}: {error}"))
        }
        Err(ResourceError::NotFound { .. }) => kinds
            .create(&empty_meta_with_labels(name, seeded), &desired)
            .await
            .map(|_| ())
            .map_err(|error| format!("seed fulfilment kind {name}: {error}")),
        Err(error) => Err(format!("inspect fulfilment kind {name}: {error}")),
    }
}

pub(super) fn kind_belongs_to_host(
    hosts: &[ResourceObject<Host>],
    kind: &ResourceObject<FulfilmentKind>,
    host_ref: &str,
    site: &'static str,
) -> bool {
    if kind.spec.host_ref == host_ref {
        return true;
    }
    match canonical_host_id(hosts, &kind.spec.host_ref) {
        Ok(id) => id.is_some_and(|id| id.as_str() == host_ref),
        Err(error) => {
            warn!(kind = %kind.metadata.name, %site, %error, "skipping ambiguous fulfilment kind host");
            false
        }
    }
}

/// Repairs this host's kinds whose host ref names it other than by its Host's name.
pub(super) async fn canonicalize_kind_hosts(backend: &ResourceBackend, namespace: &str, host_ref: &str) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace).list().await.map_err(|error| error.to_string())?.items;
    let kinds = backend.clone().using::<FulfilmentKind>(namespace);
    // Idempotent: after the first successful pass the stored ref is the Host's name.
    for kind in kinds.list().await.map_err(|error| error.to_string())?.items {
        if kind.metadata.deletion_timestamp.is_some()
            || kind.spec.host_ref == host_ref
            || is_prepared_snapshot(&kind.metadata.name, &kind.metadata.labels, PLACEMENT_SNAPSHOT_KIND)
        {
            continue;
        }
        if kind_belongs_to_host(&hosts, &kind, host_ref, "kind canonicalization") {
            let mut spec = kind.spec.clone();
            spec.host_ref = host_ref.to_string();
            kinds
                .update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec)
                .await
                .map_err(|error| format!("canonicalize fulfilment kind {}: {error}", kind.metadata.name))?;
        }
    }
    Ok(())
}

pub(super) fn empty_meta(name: &str) -> InputMeta {
    empty_meta_with_labels(name, BTreeMap::new())
}

pub(super) fn empty_meta_with_labels(name: &str, labels: BTreeMap<String, String>) -> InputMeta {
    InputMeta::builder().name(name.to_string()).labels(labels).build()
}

pub(super) fn meta_from_existing<T: flotilla_resources::Resource>(
    existing: &ResourceObject<T>,
    labels: BTreeMap<String, String>,
) -> InputMeta {
    InputMeta::builder()
        .name(existing.metadata.name.clone())
        .labels(labels)
        .annotations(existing.metadata.annotations.clone())
        .owner_references(existing.metadata.owner_references.clone())
        .finalizers(existing.metadata.finalizers.clone())
        .maybe_deletion_timestamp(existing.metadata.deletion_timestamp)
        .build()
}

pub(super) fn merged_labels(existing: &BTreeMap<String, String>, expected: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut merged = existing.clone();
    for (key, value) in expected {
        merged.insert(key.clone(), value.clone());
    }
    merged
}
