use std::collections::{BTreeMap, HashMap};

use flotilla_protocol::{qualified_path::QualifiedPath, ProviderData, RepoIdentity};
use flotilla_resources::{
    canonicalize_repo_url, descriptive_repo_slug, repo_key, Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, InputMeta,
    LifecycleAuthority, ObservedCheckoutSpec, ResourceBackend, ResourceError, AUTHORITY_LABEL, REPO_KEY_LABEL, REPO_LABEL,
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
    repo_identity: &RepoIdentity,
    providers: &ProviderData,
) -> Result<(), ResourceError> {
    let canonical_repo = canonical_repo_url(repo_identity).map_err(ResourceError::invalid)?;
    let repo_key = repo_key(&canonical_repo);
    let repo_ref = descriptive_repo_slug(&canonical_repo);
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let selector = BTreeMap::from([
        (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Observed.as_label_value().to_string()),
        (REPO_KEY_LABEL.to_string(), repo_key.clone()),
    ]);
    let mut existing: HashMap<_, _> = checkouts
        .list_matching_labels(&selector)
        .await?
        .items
        .into_iter()
        .map(|checkout| (checkout.metadata.name.clone(), checkout))
        .collect();

    for (path, checkout) in &providers.checkouts {
        let name = observed_checkout_name(&repo_key, path);
        let meta = observed_checkout_meta(&name, &repo_key, &repo_ref);
        let spec = ResourceCheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: checkout.branch.clone(),
            path: path.path.to_string_lossy().into_owned(),
            repo_ref: repo_ref.clone(),
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

fn canonical_repo_url(identity: &RepoIdentity) -> Result<String, String> {
    if matches!(identity.authority.as_str(), "local" | "unknown") {
        return Err("cannot derive observed checkout identity without a recognized repository remote".to_string());
    }
    canonicalize_repo_url(&format!("https://{}/{}", identity.authority, identity.path.trim_start_matches('/')))
}

fn observed_checkout_meta(name: &str, repo_key: &str, repo_ref: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(BTreeMap::from([(REPO_KEY_LABEL.to_string(), repo_key.to_string()), (REPO_LABEL.to_string(), repo_ref.to_string())]))
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Observed)
}

fn observed_checkout_name(repo_key: &str, path: &QualifiedPath) -> String {
    let mut hash = Sha256::new();
    hash.update(b"observed-checkout-v1\0");
    hash.update(repo_key.as_bytes());
    hash.update([0]);
    hash.update(path.to_string().as_bytes());
    let digest = format!("{:x}", hash.finalize());
    format!("checkout-{}", &digest[..54])
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{
        qualified_path::{HostId, QualifiedPath},
        HostName, ProviderData, RepoIdentity,
    };
    use flotilla_resources::{
        Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, InMemoryBackend, InputMeta, ObservedCheckoutSpec,
        ResourceBackend, ResourceError,
    };

    use super::{canonical_repo_url, observed_checkout_name, reconcile_checkouts};

    #[test]
    fn observed_checkout_names_are_stable_and_host_scoped() {
        let path = "/workspace/flotilla";
        let first = observed_checkout_name("repo-key", &QualifiedPath::host(HostId::new("host-a"), path));
        let second = observed_checkout_name("repo-key", &QualifiedPath::host(HostId::new("host-a"), path));
        let other_host = observed_checkout_name("repo-key", &QualifiedPath::from_host_name(&HostName::new("host-b"), path));

        assert_eq!(first, second);
        assert_ne!(first, other_host);
        assert!(first.len() <= 63);
    }

    #[test]
    fn canonical_repo_url_normalizes_the_remote_identity() {
        let identity = RepoIdentity { authority: "github.com".to_string(), path: "flotilla-org/flotilla".to_string() };

        assert_eq!(canonical_repo_url(&identity).expect("canonical repo URL"), "https://github.com/flotilla-org/flotilla");
    }

    #[test]
    fn canonical_repo_url_normalizes_authority_case() {
        let identity = RepoIdentity { authority: "GitHub.com".to_string(), path: "flotilla-org/flotilla".to_string() };

        assert_eq!(canonical_repo_url(&identity).expect("canonical repo URL"), "https://github.com/flotilla-org/flotilla");
    }

    #[test]
    fn canonical_repo_url_rejects_unknown_repo_identity() {
        let identity = RepoIdentity { authority: "unknown".to_string(), path: "not-a-remote".to_string() };

        assert!(canonical_repo_url(&identity).is_err());
    }

    #[test]
    fn canonical_repo_url_rejects_local_path_identity() {
        let identity = RepoIdentity { authority: "local".to_string(), path: "/Users/alice/dev/project".to_string() };

        assert!(canonical_repo_url(&identity).is_err());
    }

    #[tokio::test]
    async fn unknown_repo_identity_leaves_existing_observed_checkouts_untouched() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
        let checkouts = backend.using::<ResourceCheckout>("flotilla");
        checkouts
            .create(
                &InputMeta::builder().name("existing".to_string()).build(),
                &ResourceCheckoutSpec::Observed(ObservedCheckoutSpec {
                    r#ref: "main".to_string(),
                    path: "/workspace/repo".to_string(),
                    repo_ref: "existing-repo".to_string(),
                    is_main: true,
                }),
            )
            .await
            .expect("existing checkout should be created");

        let result = reconcile_checkouts(
            &backend,
            "flotilla",
            &RepoIdentity { authority: "unknown".to_string(), path: "not-a-remote".to_string() },
            &ProviderData::default(),
        )
        .await;

        assert!(matches!(result, Err(ResourceError::Invalid { .. })));
        assert_eq!(checkouts.list().await.expect("checkout list should succeed").items.len(), 1);
    }
}
