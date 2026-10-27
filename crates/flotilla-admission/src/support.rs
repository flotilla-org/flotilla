//! Shared admission planning and resource helpers.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::DateTime;
use chrono::Utc;
use flotilla_protocol::CanonicalHostId;
use flotilla_protocol::PlacementTargetHost;
use flotilla_resources::canonical_placement_host_ref_from_sources;
use flotilla_resources::Checkout as ResourceCheckout;
use flotilla_resources::CheckoutPhase as ResourceCheckoutPhase;
use flotilla_resources::CheckoutSpec as ResourceCheckoutSpec;
use flotilla_resources::CheckoutStatus as ResourceCheckoutStatus;
use flotilla_resources::Convoy;
use flotilla_resources::Host as ResourceHost;
use flotilla_resources::HostOccupancy;
use flotilla_resources::HostStatus as ResourceHostStatus;
use flotilla_resources::InputMeta;
use flotilla_resources::LifecycleAuthority;
use flotilla_resources::ObservedCheckoutSpec as ResourceObservedCheckoutSpec;
use flotilla_resources::PlacementPolicy;
use flotilla_resources::PlacementPolicySpec;
use flotilla_resources::ProjectSpec;
use flotilla_resources::ReadResourceObject;
use flotilla_resources::Repository;
use flotilla_resources::RepositoryKey;
use flotilla_resources::RepositorySpec;
use flotilla_resources::Resource;
use flotilla_resources::ResourceError;
use flotilla_resources::ResourceObject;
use flotilla_resources::ResourceProvenance;
use flotilla_resources::Vessel;
use flotilla_resources::WorkflowTemplate;
use flotilla_resources::WorkflowTemplateSpec;
use flotilla_resources::CONVOY_LABEL;
use flotilla_resources::{home_copies_first, pinned_workflow_ref};
use flotilla_store::ResourceBackend;
use flotilla_store::StoreQuery;
use tracing::warn;

pub fn parse_and_validate_workflow_template_yaml(yaml: &str) -> Result<WorkflowTemplateSpec, String> {
    let spec: WorkflowTemplateSpec = serde_yml::from_str(yaml).map_err(|err| format!("invalid workflow template YAML: {err}"))?;
    flotilla_resources::validate(&spec).map_err(|errors| {
        let joined = errors.iter().map(|e| format!("{e}")).collect::<Vec<_>>().join("; ");
        format!("workflow template validation failed: {joined}")
    })?;
    Ok(spec)
}

pub fn parse_project_yaml(yaml: &str) -> Result<ProjectSpec, String> {
    serde_yml::from_str(yaml).map_err(|err| format!("invalid project YAML: {err}"))
}

pub fn adopted_checkout_name(convoy_name: &str) -> String {
    format!("adopted-checkout-{convoy_name}")
}

#[derive(bon::Builder)]
pub struct AdoptedCheckoutRequest<'a> {
    namespace: &'a str,
    convoy_name: &'a str,
    checkout_path: &'a Path,
    repository_spec: &'a RepositorySpec,
    repository_url: &'a str,
    git_ref: &'a str,
    host_ref: &'a str,
}

pub async fn create_adopted_checkout_resource(
    durable_backend: &ResourceBackend,
    observed_backend: &ResourceBackend,
    request: AdoptedCheckoutRequest<'_>,
) -> Result<(String, String, String), String> {
    let AdoptedCheckoutRequest { namespace, convoy_name, checkout_path, repository_spec, repository_url, git_ref, host_ref } = request;
    let path = flotilla_provider_runtime::probe::canonicalize(checkout_path).await?;
    let path_str = path.to_string_lossy().to_string();
    let checkout_ref = adopted_checkout_name(convoy_name);
    let repository_key = repository_spec.key();
    flotilla_store::ensure_repository(&durable_backend.clone().using::<Repository>(namespace), &repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    let meta = InputMeta::builder()
        .name(checkout_ref.clone())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Adopted);
    let spec = ResourceCheckoutSpec::Observed(
        ResourceObservedCheckoutSpec::builder()
            .r#ref(git_ref.to_string())
            .path(path_str.clone())
            .repo_ref(repository_key)
            .host_ref(host_ref.to_string())
            .is_main(matches!(git_ref, "main" | "master" | "trunk"))
            .build(),
    );
    let status = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).path(path_str).build();

    let durable = persist_adopted_checkout(durable_backend, namespace, &checkout_ref, &meta, &spec, &status).await?;
    match flotilla_observation::observed_resources::project_adopted_checkout(observed_backend, namespace, &durable).await {
        Ok(()) => {}
        Err(ResourceError::Invalid { message }) => return Err(message),
        Err(error) => {
            warn!(checkout = %checkout_ref, %error, "adopted checkout committed durably but observed publication failed; reconciliation will retry");
        }
    }

    Ok((checkout_ref, repository_url.to_string(), git_ref.to_string()))
}

pub async fn persist_adopted_checkout(
    backend: &ResourceBackend,
    namespace: &str,
    checkout_ref: &str,
    meta: &InputMeta,
    spec: &ResourceCheckoutSpec,
    status: &ResourceCheckoutStatus,
) -> Result<ResourceObject<ResourceCheckout>, String> {
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let checkout = match checkouts.create(meta, spec).await {
        Ok(checkout) => checkout,
        Err(ResourceError::Conflict { .. }) => {
            let existing = checkouts.get(checkout_ref).await.map_err(|err| err.to_string())?;
            if existing.metadata.lifecycle_authority().map_err(|err| err.to_string())? != Some(LifecycleAuthority::Adopted) {
                return Err(format!("checkout {checkout_ref} already exists but is not adopted"));
            }
            if &existing.spec != spec {
                return Err(format!("checkout {checkout_ref} already exists with different adopted checkout details"));
            }
            existing
        }
        Err(err) => return Err(err.to_string()),
    };
    if checkout.status.is_some() {
        Ok(checkout)
    } else {
        checkouts.update_status(checkout_ref, &checkout.metadata.resource_version, status).await.map_err(|err| err.to_string())
    }
}

pub fn home_copy_wins_by_name<T: Resource>(sources: impl IntoIterator<Item = ReadResourceObject<T>>) -> Vec<ResourceObject<T>> {
    let mut resolved = BTreeMap::<String, ReadResourceObject<T>>::new();
    for source in sources {
        let name = source.object.metadata.name.clone();
        match resolved.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(source);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if matches!(source.provenance, ResourceProvenance::Local)
                    && matches!(entry.get().provenance, ResourceProvenance::Replica { .. }) =>
            {
                entry.insert(source);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    resolved.into_values().map(|source| source.object).collect()
}

pub fn placement_host_ref(policy: &ResourceObject<PlacementPolicy>) -> Option<&str> {
    policy
        .spec
        .host_direct
        .as_ref()
        .map(|spec| spec.host_ref.as_str())
        .or_else(|| policy.spec.docker_per_vessel.as_ref().map(|spec| spec.host_ref.as_str()))
}

pub async fn placement_target_host(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &ResourceObject<PlacementPolicy>,
) -> Result<PlacementTargetHost, String> {
    if policy.spec.cloud_sandbox_per_vessel.is_some() {
        let hosts = backend.including_replicas::<flotilla_resources::Host>(namespace).list().await.map_err(|e| e.to_string())?;
        let instance = policy.spec.provider_requirements.as_ref().and_then(|r| r.instance.as_deref()).or_else(|| {
            policy.metadata.labels.get(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL).map(String::as_str)
        });
        let observed_hosts = hosts.items.iter().map(|h| h.object.metadata.name.as_str()).collect::<Vec<_>>().join(", ");
        let mut controllers = Vec::new();
        let account_classes = [flotilla_protocol::provider::EnvironmentClass::CloudSandbox];
        for record in hosts.items {
            let host = record.object;
            let declarations: Vec<flotilla_protocol::provider::ProviderAdvertisement> = host
                .status
                .as_ref()
                .and_then(|s| s.capabilities.get(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            if declarations
                .iter()
                .any(|p| p.kind.as_ref().is_some_and(|k| account_classes.contains(&k.class)) && instance.is_none_or(|i| p.instance == i))
            {
                controllers.push(host.metadata.name);
            }
        }
        controllers.sort();
        controllers.dedup();
        let [controller] = controllers.as_slice() else {
            return Err(format!("cloud provider controller is missing or ambiguous (observed hosts: {observed_hosts})"));
        };
        return canonical_placement_host_ref(backend, namespace, controller).await?.ok_or("cloud controller host is missing".into());
    }
    let host_ref = placement_host_ref(policy).ok_or_else(|| format!("placement `{}` has no target host", policy.metadata.name))?;
    canonical_placement_host_ref(backend, namespace, host_ref)
        .await
        .and_then(|target| target.ok_or_else(|| format!("references unknown host `{host_ref}`")))
        .map_err(|error| format!("placement `{}` {error}", policy.metadata.name))
}

pub async fn canonical_placement_host_ref(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let hosts = backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
    canonical_placement_host_ref_from_sources(&hosts.items, host_ref)
}

pub async fn authoritative_placement_host(
    backend: &ResourceBackend,
    namespace: &str,
    target_host: &PlacementTargetHost,
    placement_name: &str,
) -> Result<ResourceObject<ResourceHost>, String> {
    backend
        .including_replicas::<ResourceHost>(namespace)
        .get(target_host.reference.as_str())
        .await
        .map(|source| source.object)
        .map_err(|error| format!("placement `{placement_name}` target host is not ready: {error}"))
}

pub fn host_generation(status: Option<&ResourceHostStatus>) -> &str {
    status.and_then(|status| status.daemon_generation.as_deref()).unwrap_or("unknown")
}

pub fn placement_host_not_ready_reason(placement_name: &str, host_label: &str, generation: &str, status: &ResourceHostStatus) -> String {
    let mut failing_conditions = status
        .conditions
        .iter()
        .filter(|condition| condition.blocks_readiness())
        .map(|condition| format!("{}: {}", condition.reason, condition.message))
        .collect::<Vec<_>>();
    failing_conditions.sort();
    let detail = if failing_conditions.is_empty() { String::new() } else { format!(": {}", failing_conditions.join("; ")) };
    format!("placement `{placement_name}` host `{host_label}` generation `{generation}` is not ready{detail}")
}

pub fn unready_placement_refusal(
    placement_name: &str,
    host_label: &str,
    status: Option<&ResourceHostStatus>,
    now: DateTime<Utc>,
) -> Option<String> {
    let Some(status) = status else {
        return Some(format!("placement `{placement_name}` host `{host_label}` is not ready: status is unavailable"));
    };
    if status.sleeping_until.is_some_and(|until| until > now) {
        return None;
    }
    let mut observed = status.clone();
    observed.apply_heartbeat_readiness(now);
    if observed.ready {
        return None;
    }
    let mut reason = placement_host_not_ready_reason(placement_name, host_label, host_generation(Some(status)), &observed);
    if !observed.readiness_blocked() && !status.heartbeat_is_fresh(now) {
        match observed.heartbeat_at {
            None => reason.push_str(": heartbeat is unavailable"),
            Some(_) => reason.push_str(": heartbeat is stale"),
        }
    }
    Some(reason)
}

pub fn check_placement_capacity(target_host: &PlacementTargetHost, capacity: Option<(u64, Option<u64>)>) -> Result<(), String> {
    let Some((floor_bytes, free_bytes)) = capacity else {
        return Err(format!("placement refused on host `{}`: admission free-space floor is unavailable", target_host.display_name));
    };
    if floor_bytes == 0 {
        return Ok(());
    }
    let free_bytes =
        free_bytes.ok_or_else(|| format!("placement refused on host `{}`: free space is unavailable", target_host.display_name))?;
    crate::admission::check_measured_free_space(&target_host.display_name, free_bytes, floor_bytes)
}

/// Agentless SSH hosts run crews in an account the owning daemon does not
/// control, so an uncapped one takes only convoys that name its placement
/// deliberately. A host that declares `max_vessels` takes unpinned convoys
/// within that cap (#3793).
pub const AGENTLESS_SSH_PIN_ONLY: &str = "agentless SSH host takes only convoys that pin its placement";

pub fn is_agentless_ssh_host(host: &ResourceObject<ResourceHost>) -> bool {
    matches!(host.spec.connection, flotilla_resources::HostConnection::AgentlessSsh { .. })
}

/// The vessel cap an agentless SSH host declares; other hosts have none.
pub fn agentless_vessel_cap(host: &ResourceObject<ResourceHost>) -> Option<u32> {
    is_agentless_ssh_host(host).then_some(host.spec.max_vessels).flatten()
}

/// What each of `hosts` holds against its cap, read from every convoy and
/// vessel the store sees, listed once for all of them. The owning daemon is
/// the only placer on its agentless hosts, so its store holds every convoy
/// admitted there.
pub async fn host_occupancies(
    backend: &ResourceBackend,
    namespace: &str,
    hosts: &BTreeSet<String>,
) -> Result<BTreeMap<String, HostOccupancy>, String> {
    if hosts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let query = StoreQuery { include_replicas: true, ..Default::default() };
    let convoys = home_copies_first(backend.query_declared::<Convoy>(namespace, &query).await.map_err(|error| error.to_string())?.items);
    let vessels = home_copies_first(backend.query_declared::<Vessel>(namespace, &query).await.map_err(|error| error.to_string())?.items);
    // A convoy not yet bootstrapped names its vessels only in its frozen
    // workflow snapshot record.
    let snapshots = convoys
        .iter()
        .filter(|convoy| hosts.iter().any(|host| HostOccupancy::needs_workflow_vessels(convoy, host)))
        .map(|convoy| (convoy.metadata.name.clone(), pinned_workflow_ref(convoy).to_string()))
        .collect::<BTreeMap<_, _>>();
    let templates = backend
        .definitions::<WorkflowTemplate>(namespace)
        .list_names(snapshots.values().cloned())
        .await
        .map_err(|error| error.to_string())?;
    let templates = templates.into_iter().map(|template| (template.metadata.name.clone(), template)).collect::<BTreeMap<_, _>>();
    let workflow_vessels = snapshots
        .into_iter()
        // A convoy whose snapshot record is missing is left out, and `count`
        // falls back to its pins and convoy-wide decision.
        .filter_map(|(convoy, template)| {
            let Some(template) = templates.get(template.as_str()) else {
                warn!(%convoy, %template, "workflow snapshot missing; counting the convoy's pins and convoy-wide decision instead");
                return None;
            };
            Some((convoy, template.spec.vessels.iter().map(|vessel| vessel.name.clone()).collect()))
        })
        .collect();
    Ok(hosts.iter().map(|host| (host.clone(), HostOccupancy::count(host, &convoys, &vessels, &workflow_vessels))).collect())
}

pub async fn host_occupancy(backend: &ResourceBackend, namespace: &str, host: &str) -> Result<HostOccupancy, String> {
    let mut occupancies = host_occupancies(backend, namespace, &BTreeSet::from([host.to_string()])).await?;
    Ok(occupancies.remove(host).unwrap_or_default())
}

pub async fn policy_targets_agentless_ssh(backend: &ResourceBackend, namespace: &str, policy: &ResourceObject<PlacementPolicy>) -> bool {
    let Ok(target) = placement_target_host(backend, namespace, policy).await else {
        return false;
    };
    authoritative_placement_host(backend, namespace, &target, &policy.metadata.name)
        .await
        .ok()
        .is_some_and(|host| is_agentless_ssh_host(&host))
}

/// Validate and freeze one provider for this vessel's placement snapshot.
pub async fn resolve_placement_provider(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &mut ResourceObject<PlacementPolicy>,
) -> Result<Option<String>, String> {
    use flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
    use flotilla_protocol::provider::ProviderAdvertisement;
    let target = placement_target_host(backend, namespace, policy).await?;
    let host = authoritative_placement_host(backend, namespace, &target, &policy.metadata.name).await?;
    let value = host
        .status
        .as_ref()
        .and_then(|status| status.capabilities.get(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY))
        .ok_or_else(|| format!("{}: provider advertisements not observed", target.display_name))?;
    let providers: Vec<ProviderAdvertisement> = serde_json::from_value(value.clone())
        .map_err(|error| format!("{}: invalid provider advertisements: {error}", target.display_name))?;
    // The host's provider settings decide its checkout, and name its instance when the policy
    // does not (#3824 layer 4, ADR 0058).
    let settings = flotilla_store::provider_settings(backend, namespace, &policy.spec).await?;
    let mut matched = policy.spec.clone();
    if let (Some(docker), flotilla_resources::environment_provider::ProviderSettings::DockerPerVessel { checkout, .. }) =
        (matched.docker_per_vessel.as_mut(), &settings.settings)
    {
        docker.checkout = checkout.clone();
    }
    if let Some(instance) = settings.instance.filter(|_| !policy.metadata.labels.contains_key(ENVIRONMENT_PROVIDER_INSTANCE_LABEL)) {
        matched.provider_requirements.get_or_insert_with(Default::default).instance.get_or_insert(instance);
    }
    let selected = crate::provider_placement::match_provider(
        &target.display_name,
        &matched,
        policy.metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL).map(String::as_str),
        &providers,
    )?;
    // Specs are frozen per vessel; metadata is also retained for existing image
    // cache and provisioning consumers that already use the identity label.
    policy.spec.provider_requirements.get_or_insert_with(Default::default).instance = Some(selected.instance.clone());
    policy.metadata.labels.insert(ENVIRONMENT_PROVIDER_INSTANCE_LABEL.into(), selected.instance.clone());
    if policy.spec.cloud_sandbox_per_vessel.is_some() {
        // Cloud roots name the POSIX guest, independent of the admitting host OS.
        if !selected.capabilities.workspace_root.to_string_lossy().starts_with('/') {
            return Err("cloud provider workspace root must be absolute".into());
        }
        return Ok(Some(selected.capabilities.workspace_root.to_string_lossy().into_owned()));
    }
    Ok(None)
}

/// Freeze only cloud guest roots in placement decisions, never metadata labels.
pub async fn provider_workspace_root(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &ResourceObject<PlacementPolicy>,
) -> Result<Option<String>, String> {
    if policy.spec.cloud_sandbox_per_vessel.is_none() {
        return Ok(None);
    }
    resolve_placement_provider(backend, namespace, &mut policy.clone()).await
}

pub async fn placement_actuator_host_ref(
    backend: &ResourceBackend,
    namespace: &str,
    target: &PlacementTargetHost,
) -> Result<CanonicalHostId, String> {
    let host = authoritative_placement_host(backend, namespace, target, "actuator routing").await?;
    match host.spec.connection {
        flotilla_resources::HostConnection::AgentlessSsh { owning_daemon, .. } => Ok(CanonicalHostId::resolved(owning_daemon)),
        flotilla_resources::HostConnection::Daemon => Ok(target.reference.clone()),
    }
}

/// Resolve one remote independently; callers choose which remote's forge to carry.
pub fn repository_matches_target(repository: &ResourceObject<Repository>, target: &str) -> bool {
    repository.metadata.name == target || repository.spec.matches_catalog_target(target)
}

pub fn prepared_snapshot_name(kind: &str, spec: &serde_json::Value) -> Result<String, String> {
    let suffix = flotilla_resources::content_hash(spec).map_err(|error| error.to_string())?;
    Ok(format!("{kind}-snapshot-{suffix}"))
}

pub async fn ensure_prepared_workflow_snapshot(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: &WorkflowTemplateSpec,
) -> Result<(), String> {
    let templates = backend.definitions::<WorkflowTemplate>(namespace);
    match templates.get(name).await {
        Ok(existing) if existing.spec == *spec => Ok(()),
        Ok(_) => Err(format!("prepared workflow snapshot {name} already exists with different contents")),
        Err(ResourceError::NotFound { .. }) => templates
            .apply(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(
                        flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                        flotilla_resources::WORKFLOW_SNAPSHOT_KIND.to_string(),
                    )]))
                    .build(),
                spec,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

pub async fn ensure_prepared_placement_snapshot(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: &PlacementPolicySpec,
) -> Result<(), String> {
    let policies = backend.including_replicas::<PlacementPolicy>(namespace);
    match policies.get(name).await {
        Ok(existing) if existing.object.spec == *spec => Ok(()),
        Ok(_) => Err(format!("prepared placement snapshot {name} already exists with different contents")),
        Err(ResourceError::NotFound { .. }) => backend
            .clone()
            .using::<PlacementPolicy>(namespace)
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(
                        flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                        flotilla_resources::PLACEMENT_SNAPSHOT_KIND.to_string(),
                    )]))
                    .build(),
                spec,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

pub async fn ensure_repository_and_default_project_workflow(
    backend: &ResourceBackend,
    namespace: &str,
    repository_key: &RepositoryKey,
    repository_spec: &RepositorySpec,
) -> Result<(), String> {
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(namespace), repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    flotilla_store::PreparedSnapshotGarbageCollector::new(backend.clone(), namespace)
        .collect(None)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}
