//! Startup resource seeding and placement-policy migration.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_core::{in_process::InProcessDaemon, placement_policy::reconcile_registered_policy};
use flotilla_resources::{
    canonical_host_id, descriptive_repo_slug, is_prepared_snapshot, Clone, CloneSpec, DockerCheckoutStrategy,
    DockerPerVesselPlacementPolicySpec, Environment, EnvironmentSpec, FulfilmentKind, FulfilmentKindSpec, Host, HostDirectEnvironmentSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, InputMeta, PlacementPolicy, PlacementPolicySpec,
    Repository, ResourceBackend, ResourceError, ResourceObject, WorkflowTemplate, MANAGED_BY_LABEL, PLACEMENT_SNAPSHOT_KIND,
};
use tracing::warn;

use super::discovery::{LocalProvisioningProfile, DEFAULT_DOCKER_IMAGE};

pub(super) async fn register_startup_resources(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let backend = daemon.resource_backend();
    ensure_host_exists(&backend, namespace, &profile.host_id, &profile.display_name).await?;
    ensure_host_direct_environment_exists(&backend, namespace, profile).await?;
    discover_local_clones(daemon, &backend, namespace, profile).await?;
    ensure_default_policies(&backend, namespace, profile).await?;
    reconcile_builtin_workflow_templates(&backend, namespace).await?;
    Ok(())
}

pub(super) fn mark_builtin_managed(mut meta: InputMeta) -> InputMeta {
    meta.labels.insert(MANAGED_BY_LABEL.to_string(), BUILTIN_MANAGED_BY_VALUE.to_string());
    meta
}

/// Reconciles code-owned manifests as the builtin special case of the ruled
/// manifest loop in https://github.com/flotilla-org/flotilla/issues/1192.
pub(super) async fn reconcile_builtin_workflow_templates(backend: &ResourceBackend, namespace: &str) -> Result<(), String> {
    let templates = backend.clone().definitions::<WorkflowTemplate>(namespace);
    let builtins = flotilla_resources::builtin_workflow_templates();
    // Ownership follows the current code-owned set. Tombstone retired definitions
    // through the merged resolver so replicas cannot resurrect stale builtins.
    for existing in templates.list().await.map_err(|err| format!("list builtin workflow templates: {err}"))? {
        if existing.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == BUILTIN_MANAGED_BY_VALUE)
            && !builtins.iter().any(|(name, _)| *name == existing.metadata.name)
        {
            templates
                .delete(&existing.metadata.name)
                .await
                .map_err(|err| format!("retire builtin workflow template {}: {err}", existing.metadata.name))?;
            warn!(template = %existing.metadata.name, "retired orphaned builtin workflow template");
        }
    }
    for (name, spec) in builtins {
        match templates.get(name).await {
            Ok(existing) => {
                let spec_diverged = existing.spec != spec;
                let managed_by_builtin =
                    existing.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == BUILTIN_MANAGED_BY_VALUE);
                if !spec_diverged && managed_by_builtin {
                    continue;
                }
                if !spec_diverged {
                    templates
                        .apply(&mark_builtin_managed(InputMeta::from(&existing.metadata)), &spec)
                        .await
                        .map_err(|err| format!("reconcile builtin workflow template {name}: {err}"))?;
                    continue;
                }
                templates
                    .apply(&mark_builtin_managed(InputMeta::from(&existing.metadata)), &spec)
                    .await
                    .map_err(|err| format!("reconcile builtin workflow template {name}: {err}"))?;
                warn!(template = %name, "stored spec diverged from code builtin; overwriting");
            }
            Err(ResourceError::NotFound { .. }) => {
                templates
                    .apply(&mark_builtin_managed(empty_meta(name)), &spec)
                    .await
                    .map_err(|err| format!("seed builtin workflow template {name}: {err}"))?;
            }
            Err(err) => return Err(format!("check workflow template {name}: {err}")),
        }
    }
    Ok(())
}

pub(super) async fn ensure_host_exists(
    backend: &ResourceBackend,
    namespace: &str,
    host_name: &str,
    display_name: &str,
) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace);
    match hosts.get(host_name).await {
        Ok(existing) if existing.spec.display_name == display_name => return Ok(()),
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
                    },
                )
                .await
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
        Err(ResourceError::NotFound { .. }) => {}
        Err(err) => return Err(format!("check host {host_name}: {err}")),
    }
    hosts
        .create(
            &empty_meta(host_name),
            &HostSpec { display_name: display_name.to_string(), connection: Default::default(), ..HostSpec::default() },
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

    environments
        .create(
            &empty_meta(&name),
            &EnvironmentSpec {
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
        flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, &repository_spec)
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

pub(super) async fn ensure_default_policies(
    backend: &ResourceBackend,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let host_direct_name = profile.host_direct_policy_name();
    reconcile_registered_policy(
        backend,
        namespace,
        &host_direct_name,
        &PlacementPolicySpec::builder()
            .pool(profile.host_direct_pool.clone())
            .host_direct(HostDirectPlacementPolicySpec {
                host_ref: profile.host_id.clone(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            })
            .build(),
    )
    .await?;

    if profile.docker_available {
        let docker_name = profile.docker_policy_name();
        reconcile_registered_policy(
            backend,
            namespace,
            &docker_name,
            &PlacementPolicySpec::builder()
                .pool(profile.docker_pool.clone())
                .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: profile.host_id.clone(),
                    image: DEFAULT_DOCKER_IMAGE.to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::new(),
                    default_cwd: Some("/workspace".to_string()),
                    env: BTreeMap::new(),
                    checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await?;
    }

    migrate_live_placement_policies(backend, namespace, &profile.host_id, std::env::consts::OS).await
}

/// A1 runs beside the policy-based admission path. Existing policy names are
/// retained so operators can inspect the corresponding kind during rollout.
pub(super) async fn migrate_live_placement_policies(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
    platform: &str,
) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace).list().await.map_err(|error| error.to_string())?;
    let policies =
        backend.clone().using::<flotilla_resources::PlacementPolicy>(namespace).list().await.map_err(|error| error.to_string())?;
    migrate_listed_placement_policies(backend, namespace, host_ref, platform, &hosts.items, policies.items).await
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

pub(super) async fn migrate_listed_placement_policies(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
    platform: &str,
    hosts: &[ResourceObject<Host>],
    policies: Vec<ResourceObject<PlacementPolicy>>,
) -> Result<(), String> {
    let kinds = backend.clone().using::<FulfilmentKind>(namespace);
    for policy in policies {
        let policy_host = policy
            .spec
            .host_direct
            .as_ref()
            .map(|strategy| strategy.host_ref.as_str())
            .or_else(|| policy.spec.docker_per_vessel.as_ref().map(|strategy| strategy.host_ref.as_str()));
        let Some(policy_host) = policy_host else { continue };
        let canonical = match canonical_host_id(hosts, policy_host) {
            Ok(id) => id.map(|id| id.to_string()).unwrap_or_else(|| policy_host.to_string()),
            Err(error) => {
                warn!(policy = %policy.metadata.name, %policy_host, %error, "skipping ambiguous placement policy host");
                continue;
            }
        };
        if canonical != host_ref {
            continue;
        }
        if is_prepared_snapshot(&policy.metadata.name, &policy.metadata.labels, PLACEMENT_SNAPSHOT_KIND) {
            // Earlier migrations created kinds under frozen policy names. A
            // matching snapshot policy is the proof of origin for cleanup;
            // leave the policy itself for existing convoy references.
            match kinds.get(&policy.metadata.name).await {
                Ok(kind) => {
                    if kind_belongs_to_host(hosts, &kind, host_ref, "snapshot cleanup") {
                        kinds.delete(&policy.metadata.name).await.map_err(|error| format!("delete snapshot fulfilment kind: {error}"))?;
                    }
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(format!("inspect snapshot fulfilment kind: {error}")),
            }
            continue;
        }
        if policy.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let mut policy_spec = policy.spec.clone();
        if policy_host != host_ref {
            // One-time repair of display-name refs written before host IDs
            // became the stored PlacementPolicy/FulfilmentKind identity.
            if let Some(direct) = policy_spec.host_direct.as_mut() {
                direct.host_ref = host_ref.to_string();
            }
            if let Some(docker) = policy_spec.docker_per_vessel.as_mut() {
                docker.host_ref = host_ref.to_string();
            }
        }
        let name = &policy.metadata.name;
        let spec = match FulfilmentKindSpec::from_policy(&policy_spec, platform) {
            Ok(spec) => spec,
            Err(error) => {
                warn!(policy = %name, %error, "skipping invalid placement policy during fulfilment migration");
                continue;
            }
        };
        if policy_spec != policy.spec {
            match backend
                .clone()
                .using::<flotilla_resources::PlacementPolicy>(namespace)
                .update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy_spec)
                .await
            {
                Ok(_) => {}
                Err(ResourceError::Conflict { .. }) => {
                    warn!(policy = %name, "placement policy changed during host-ref canonicalization; retrying next pass");
                    continue;
                }
                Err(error) => return Err(format!("canonicalize placement policy {name}: {error}")),
            }
        }
        match kinds.get(name).await {
            Ok(existing) if existing.metadata.deletion_timestamp.is_some() => {}
            Ok(existing) => {
                let mut updated = spec.clone();
                updated.grants = existing.spec.grants.clone();
                updated.cost_class = existing.spec.cost_class;
                // Platform is a host fact. Correct an initial local-OS
                // registration when an agentless SSH host reports its own OS.
                updated.grants.retain(|grant| !grant.0.starts_with("platform:"));
                updated.grants.extend(spec.grants.iter().filter(|grant| grant.0.starts_with("platform:")).cloned());
                if updated != existing.spec {
                    kinds
                        .update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &updated)
                        .await
                        .map_err(|error| format!("update fulfilment kind {name}: {error}"))?;
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                kinds.create(&empty_meta(name), &spec).await.map_err(|error| format!("create fulfilment kind {name}: {error}"))?;
            }
            Err(error) => return Err(format!("inspect fulfilment kind {name}: {error}")),
        }
    }
    // Also repair kinds that were authored directly and have no live policy.
    // This is idempotent; after the first successful pass the stored ref is
    // already the canonical Host resource name.
    for kind in kinds.list().await.map_err(|error| error.to_string())?.items {
        if kind.metadata.deletion_timestamp.is_some() || kind.spec.host_ref == host_ref {
            continue;
        }
        if kind_belongs_to_host(hosts, &kind, host_ref, "kind canonicalization") {
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

pub(super) const BUILTIN_MANAGED_BY_VALUE: &str = "builtin";
