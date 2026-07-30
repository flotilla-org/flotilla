use flotilla_resources::{
    InputMeta, PlacementPolicy, PlacementPolicySpec, ResourceBackend, ResourceError, WriterIdentity, MANAGED_BY_LABEL,
};
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
                    .write_spec(
                        &WriterIdentity::reconcile_loop(),
                        &InputMeta::from(&existing.metadata),
                        &existing.metadata.resource_version,
                        &merged,
                    )
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
    // configuration. Starting from the live spec means the ownership-aware
    // write only expresses intent for fields the registration loop controls.
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use flotilla_resources::{
        DockerCheckoutStrategy, DockerImagePullPolicy, DockerPerVesselPlacementPolicySpec, HostDirectPlacementPolicyCheckout,
        HostDirectPlacementPolicySpec, InMemoryBackend,
    };

    use super::*;

    const NAMESPACE: &str = "flotilla";

    fn host_direct(pool: &str, host: &str) -> PlacementPolicySpec {
        PlacementPolicySpec::builder()
            .pool(pool.to_string())
            .host_direct(HostDirectPlacementPolicySpec {
                host_ref: host.to_string(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            })
            .build()
    }

    fn docker(pool: &str, host: &str) -> PlacementPolicySpec {
        PlacementPolicySpec::builder()
            .pool(pool.to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                host_ref: host.to_string(),
                image: "registration-default:latest".to_string(),
                pull_policy: DockerImagePullPolicy::IfNotPresent,
                agent_adapters: BTreeSet::new(),
                default_cwd: Some("/workspace".to_string()),
                env: BTreeMap::new(),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
            })
            .build()
    }

    #[tokio::test]
    async fn repeated_registration_preserves_operator_priority_without_false_violations() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let policies = backend.using::<PlacementPolicy>(NAMESPACE);
        reconcile_registered_policy(&backend, NAMESPACE, "host-direct-shared", &host_direct("local", "local"))
            .await
            .expect("initial registration");
        let created = policies.get("host-direct-shared").await.expect("registered policy");
        let mut operator_spec = created.spec.clone();
        operator_spec.priority = 73;
        policies
            .write_spec(
                &WriterIdentity::operator(),
                &InputMeta::from(&created.metadata),
                &created.metadata.resource_version,
                &operator_spec,
            )
            .await
            .expect("operator applies priority");

        for cycle in 0..256 {
            let (pool, host) = if cycle % 2 == 0 { ("local", "local") } else { ("remote", "remote") };
            reconcile_registered_policy(&backend, NAMESPACE, "host-direct-shared", &host_direct(pool, host))
                .await
                .expect("registration cycle");
            assert_eq!(
                policies.get("host-direct-shared").await.expect("registered policy").spec.priority,
                73,
                "priority changed during registration cycle {cycle}"
            );
        }

        let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics");
        assert!(diagnostics.field_ownership_violations.is_empty(), "registration must not claim intent for operator fields");
    }

    #[tokio::test]
    async fn docker_registration_preserves_operator_runtime_configuration_without_false_violations() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let policies = backend.using::<PlacementPolicy>(NAMESPACE);
        reconcile_registered_policy(&backend, NAMESPACE, "docker-shared", &docker("local", "local")).await.expect("initial registration");
        let created = policies.get("docker-shared").await.expect("registered policy");
        let mut operator_spec = created.spec.clone();
        let runtime = operator_spec.docker_per_vessel.as_mut().expect("docker strategy");
        runtime.image = "operator/custom:latest".to_string();
        runtime.pull_policy = DockerImagePullPolicy::Never;
        runtime.agent_adapters = BTreeSet::from(["codex".to_string()]);
        runtime.default_cwd = Some("/operator-workspace".to_string());
        runtime.env = BTreeMap::from([("OPERATOR".to_string(), "true".to_string())]);
        policies
            .write_spec(
                &WriterIdentity::operator(),
                &InputMeta::from(&created.metadata),
                &created.metadata.resource_version,
                &operator_spec,
            )
            .await
            .expect("operator configures runtime");

        reconcile_registered_policy(&backend, NAMESPACE, "docker-shared", &docker("remote", "remote")).await.expect("registration refresh");
        let refreshed = policies.get("docker-shared").await.expect("refreshed policy");
        let runtime = refreshed.spec.docker_per_vessel.expect("docker strategy");

        assert_eq!(runtime.host_ref, "remote");
        assert_eq!(runtime.image, "operator/custom:latest");
        assert_eq!(runtime.pull_policy, DockerImagePullPolicy::Never);
        assert_eq!(runtime.agent_adapters, BTreeSet::from(["codex".to_string()]));
        assert_eq!(runtime.default_cwd.as_deref(), Some("/operator-workspace"));
        assert_eq!(runtime.env, BTreeMap::from([("OPERATOR".to_string(), "true".to_string())]));
        let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics");
        assert!(diagnostics.field_ownership_violations.is_empty(), "registration must not synthesize runtime write attempts");
    }
}
