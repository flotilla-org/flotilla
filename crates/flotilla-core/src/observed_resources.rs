use std::collections::{BTreeMap, HashMap};

use flotilla_protocol::{qualified_path::QualifiedPath, ProviderData};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, InputMeta, LifecycleAuthority, ObservedCheckoutSpec, RepositoryKey,
    ResourceBackend, ResourceError, AUTHORITY_LABEL, REPO_KEY_LABEL, REPO_LABEL,
};
use sha2::{Digest, Sha256};

/// Publish the checkout facts discovered for one local repository into the
/// daemon's ephemeral observed-resource store.
///
/// The legacy `ProviderData` remains the input during the observer reshape,
/// so it can continue feeding Plane A while the same refresh projects its
/// checkout facts into the resource store.
pub async fn reconcile_checkouts(
    backend: &ResourceBackend,
    namespace: &str,
    repository_key: &RepositoryKey,
    repository_slug: &str,
    providers: &ProviderData,
    host_ref: &str,
) -> Result<(), ResourceError> {
    let scope = ObservedCheckoutScope { repo_key: repository_key.clone(), repo_slug: repository_slug.to_string() };
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let selector = scope.selector();
    let mut existing: HashMap<_, _> = checkouts
        .list_matching_labels(&selector)
        .await?
        .items
        .into_iter()
        .map(|checkout| (checkout.metadata.name.clone(), checkout))
        .collect();

    for (path, checkout) in &providers.checkouts {
        let name = observed_checkout_name(&scope.repo_key, path);
        let meta = observed_checkout_meta(&name, &scope.repo_key, &scope.repo_slug);
        let spec = ResourceCheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: checkout.branch.clone(),
            path: path.path.to_string_lossy().into_owned(),
            repo_ref: scope.repo_key.clone(),
            host_ref: host_ref.to_string(),
            is_main: checkout.is_main,
        });

        match existing.remove(&name) {
            Some(current) if current.spec == spec && current.metadata.labels == meta.labels => {}
            Some(current) => {
                checkouts.update(&meta, &current.metadata.resource_version, &spec).await?;
            }
            None => {
                checkouts.create(&meta, &spec).await?;
            }
        }
    }

    for stale_name in existing.into_keys() {
        checkouts.delete(&stale_name).await?;
    }

    Ok(())
}

/// Delete the Checkout facts previously published for one local repository.
///
/// Adopted and managed Checkouts are outside this projection's lifecycle, so
/// cleanup is restricted to resources carrying the observed authority label.
pub async fn delete_observed_checkouts(
    backend: &ResourceBackend,
    namespace: &str,
    repository_key: &RepositoryKey,
) -> Result<(), ResourceError> {
    let scope = ObservedCheckoutScope { repo_key: repository_key.clone(), repo_slug: String::new() };
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let selector = scope.selector();

    for checkout in checkouts.list_matching_labels(&selector).await?.items {
        checkouts.delete(&checkout.metadata.name).await?;
    }

    Ok(())
}

struct ObservedCheckoutScope {
    repo_key: RepositoryKey,
    repo_slug: String,
}

impl ObservedCheckoutScope {
    fn selector(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Observed.as_label_value().to_string()),
            (REPO_KEY_LABEL.to_string(), self.repo_key.to_string()),
        ])
    }
}

fn observed_checkout_meta(name: &str, repo_key: &RepositoryKey, repo_slug: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(BTreeMap::from([(REPO_KEY_LABEL.to_string(), repo_key.to_string()), (REPO_LABEL.to_string(), repo_slug.to_string())]))
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Observed)
}

fn observed_checkout_name(repo_key: &RepositoryKey, path: &QualifiedPath) -> String {
    let mut hash = Sha256::new();
    hash.update(b"observed-checkout-v1\0");
    hash.update(repo_key.0.as_bytes());
    hash.update([0]);
    hash.update(path.to_string().as_bytes());
    let digest = format!("{:x}", hash.finalize());
    format!("checkout-{}", &digest[..54])
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{
        qualified_path::{HostId, QualifiedPath},
        Checkout, HostName, ProviderData,
    };
    use flotilla_resources::{Checkout as ResourceCheckout, InMemoryBackend, RepositoryKey, ResourceBackend};

    use super::{observed_checkout_name, reconcile_checkouts};

    #[test]
    fn observed_checkout_names_are_stable_and_host_scoped() {
        let path = "/workspace/flotilla";
        let repo_key = RepositoryKey("repo-key".to_string());
        let first = observed_checkout_name(&repo_key, &QualifiedPath::host(HostId::new("host-a"), path));
        let second = observed_checkout_name(&repo_key, &QualifiedPath::host(HostId::new("host-a"), path));
        let other_host = observed_checkout_name(&repo_key, &QualifiedPath::from_host_name(&HostName::new("host-b"), path));

        assert_eq!(first, second);
        assert_ne!(first, other_host);
        assert!(first.len() <= 63);
    }

    #[tokio::test]
    async fn explicit_repository_identity_supports_remote_less_observations() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
        let checkouts = backend.using::<ResourceCheckout>("flotilla");
        let repository_key = RepositoryKey("local-repository".to_string());
        let path = QualifiedPath::host(HostId::new("host-01"), "/workspace/repo");
        let providers = ProviderData {
            checkouts: [(path, Checkout {
                branch: "main".to_string(),
                is_main: true,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: Vec::new(),
                association_keys: Vec::new(),
                host_name: None,
                environment_id: None,
            })]
            .into_iter()
            .collect(),
            ..ProviderData::default()
        };

        reconcile_checkouts(&backend, "flotilla", &repository_key, "local-repo", &providers, "host-01")
            .await
            .expect("remote-less observation should reconcile");

        let stored = checkouts.list().await.expect("checkout list should succeed").items;
        assert!(matches!(stored.as_slice(), [checkout] if checkout.spec.repo_ref() == &repository_key));
    }
}
