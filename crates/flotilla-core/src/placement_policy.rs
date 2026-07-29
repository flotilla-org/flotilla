use flotilla_resources::{InputMeta, PlacementPolicy, PlacementPolicySpec, ResourceBackend, ResourceError, MANAGED_BY_LABEL};
use tracing::warn;

/// Reconcile a daemon-discovered placement policy without claiming
/// operator-authored fields or manifest-managed resources.
pub async fn reconcile_registered_policy(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    desired: &PlacementPolicySpec,
) -> Result<(), String> {
    let policies = backend.clone().using::<PlacementPolicy>(namespace);
    match policies.get(name).await {
        Ok(existing) => {
            if existing.metadata.deletion_timestamp.is_some() {
                return Ok(());
            }
            if let Some(managed_by) = existing.metadata.labels.get(MANAGED_BY_LABEL) {
                warn!(policy = %name, %managed_by, "leaving managed placement policy untouched during daemon registration");
                return Ok(());
            }

            let merged = merge_registered_policy_spec(&existing.spec, desired)?;
            if merged != existing.spec {
                policies
                    .update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &merged)
                    .await
                    .map_err(|error| format!("reconcile registered placement policy {name}: {error}"))?;
            }
            Ok(())
        }
        Err(ResourceError::NotFound { .. }) => policies
            .create(&InputMeta::builder().name(name.to_string()).build(), desired)
            .await
            .map(|_| ())
            .map_err(|error| format!("register placement policy {name}: {error}")),
        Err(error) => Err(format!("check registered placement policy {name}: {error}")),
    }
}

fn merge_registered_policy_spec(existing: &PlacementPolicySpec, desired: &PlacementPolicySpec) -> Result<PlacementPolicySpec, String> {
    // Registration owns placement topology, not operator scheduling or runtime
    // configuration. Starting from the live spec also preserves future fields
    // until their ownership is explicitly assigned here.
    let mut merged = existing.clone();
    merged.pool.clone_from(&desired.pool);
    match (&desired.host_direct, &desired.docker_per_vessel) {
        (Some(host_direct), None) => {
            merged.host_direct = Some(host_direct.clone());
            merged.docker_per_vessel = None;
        }
        (None, Some(desired_docker)) => {
            let mut docker = existing.docker_per_vessel.clone().unwrap_or_else(|| desired_docker.clone());
            docker.host_ref.clone_from(&desired_docker.host_ref);
            docker.checkout.clone_from(&desired_docker.checkout);
            merged.host_direct = None;
            merged.docker_per_vessel = Some(docker);
        }
        _ => return Err("registered placement policy must define exactly one strategy".to_string()),
    }
    Ok(merged)
}
