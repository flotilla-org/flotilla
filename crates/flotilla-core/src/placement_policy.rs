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

            if desired != &existing.spec {
                policies
                    .write_spec(
                        &WriterIdentity::reconcile_loop(),
                        &InputMeta::from(&existing.metadata),
                        &existing.metadata.resource_version,
                        desired,
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
