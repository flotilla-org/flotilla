use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use flotilla_protocol::{qualified_path::QualifiedPath, ProviderData};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus, InputMeta, LifecycleAuthority,
    ObservedCheckoutSpec, RepositoryKey, ResourceBackend, ResourceError, ResourceObject, AUTHORITY_LABEL, REPO_KEY_LABEL, REPO_LABEL,
};
use sha2::{Digest, Sha256};

/// Rebuild the query-facing adopted Checkout projection from the durable
/// controller-facing resources.
///
/// Durable resources are authoritative. A missing durable status means the
/// create was interrupted between its spec and status writes; an adopted
/// observed Checkout requires no actuation, so its Ready status can be derived
/// from the path already recorded in its spec.
pub async fn reconcile_adopted_checkouts(
    durable_backend: &ResourceBackend,
    observed_backend: &ResourceBackend,
    namespace: &str,
) -> Result<(), ResourceError> {
    let selector = BTreeMap::from([(AUTHORITY_LABEL.to_string(), LifecycleAuthority::Adopted.as_label_value().to_string())]);
    let durable_checkouts = durable_backend.clone().using::<ResourceCheckout>(namespace);
    let observed_checkouts = observed_backend.clone().using::<ResourceCheckout>(namespace);
    let mut failures = Vec::new();

    for checkout in durable_checkouts.list_matching_labels(&selector).await?.items {
        let name = checkout.metadata.name.clone();
        let result = async {
            let checkout = ensure_adopted_checkout_status(&durable_checkouts, checkout).await?;
            project_adopted_checkout_with(&observed_checkouts, &checkout).await
        }
        .await;
        if let Err(error) = result {
            failures.push(format!("{name}: {error}"));
        }
    }

    if let Err(error) = delete_stale_adopted_checkouts(durable_backend, observed_backend, namespace).await {
        failures.push(error.to_string());
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(ResourceError::other(format!("failed to reconcile adopted checkouts: {}", failures.join("; "))))
    }
}

/// Remove adopted projections whose durable adopted source no longer exists.
/// Callers serialize this with adoption and projection so a stale reconcile
/// cannot republish a checkout after its deletion.
pub async fn delete_stale_adopted_checkouts(
    durable_backend: &ResourceBackend,
    observed_backend: &ResourceBackend,
    namespace: &str,
) -> Result<(), ResourceError> {
    let selector = BTreeMap::from([(AUTHORITY_LABEL.to_string(), LifecycleAuthority::Adopted.as_label_value().to_string())]);
    let durable_checkouts = durable_backend.clone().using::<ResourceCheckout>(namespace);
    let observed_checkouts = observed_backend.clone().using::<ResourceCheckout>(namespace);
    let mut failures = Vec::new();
    for checkout in observed_checkouts.list_matching_labels(&selector).await?.items {
        let name = &checkout.metadata.name;
        let result = async {
            match durable_checkouts.get(name).await {
                Ok(source) if source.metadata.lifecycle_authority()? == Some(LifecycleAuthority::Adopted) => return Ok(()),
                Ok(_) | Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
            match observed_checkouts.delete(name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
                Err(error) => Err(error),
            }
        }
        .await;
        if let Err(error) = result {
            failures.push(format!("{name}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ResourceError::other(format!("failed to delete stale adopted checkouts: {}", failures.join("; "))))
    }
}

/// Publish one durable adopted Checkout into the ephemeral observed store.
pub async fn project_adopted_checkout(
    observed_backend: &ResourceBackend,
    namespace: &str,
    durable: &ResourceObject<ResourceCheckout>,
) -> Result<(), ResourceError> {
    project_adopted_checkout_with(&observed_backend.clone().using::<ResourceCheckout>(namespace), durable).await
}

async fn ensure_adopted_checkout_status(
    checkouts: &flotilla_resources::TypedResolver<ResourceCheckout>,
    checkout: ResourceObject<ResourceCheckout>,
) -> Result<ResourceObject<ResourceCheckout>, ResourceError> {
    if checkout.status.is_some() {
        return Ok(checkout);
    }
    let ResourceCheckoutSpec::Observed(spec) = &checkout.spec else {
        return Err(ResourceError::invalid(format!("adopted checkout {} must use an observed checkout spec", checkout.metadata.name)));
    };
    let status = CheckoutStatus::builder().phase(CheckoutPhase::Ready).path(spec.path.clone()).build();
    checkouts.update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &status).await
}

async fn project_adopted_checkout_with(
    checkouts: &flotilla_resources::TypedResolver<ResourceCheckout>,
    durable: &ResourceObject<ResourceCheckout>,
) -> Result<(), ResourceError> {
    let meta = InputMeta::from(&durable.metadata);
    let projected = match checkouts.create(&meta, &durable.spec).await {
        Ok(created) => created,
        Err(ResourceError::Conflict { .. }) => {
            let existing = checkouts.get(&durable.metadata.name).await?;
            if existing.metadata.lifecycle_authority()? != Some(LifecycleAuthority::Adopted) {
                return Err(ResourceError::invalid(format!(
                    "checkout {} already exists in the observed store but is not adopted",
                    durable.metadata.name
                )));
            }
            checkouts.update(&meta, &existing.metadata.resource_version, &durable.spec).await?
        }
        Err(error) => return Err(error),
    };

    if let Some(status) = &durable.status {
        checkouts.update_status(&durable.metadata.name, &projected.metadata.resource_version, status).await?;
    }
    Ok(())
}

/// Publish the checkout facts discovered for one local repository into the
/// daemon's ephemeral observed-resource store.
///
/// `ProviderData` is the on-demand provider interchange input. This projection
/// publishes its checkout facts as resources for the Aggregator; it does not
/// feed a separate provider snapshot pipeline.
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

pub async fn delete_observed_checkout_at_path(
    backend: &ResourceBackend,
    namespace: &str,
    repository_key: &RepositoryKey,
    path: &Path,
) -> Result<(), ResourceError> {
    let scope = ObservedCheckoutScope { repo_key: repository_key.clone(), repo_slug: String::new() };
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    for checkout in checkouts.list_matching_labels(&scope.selector()).await?.items {
        if matches!(&checkout.spec, ResourceCheckoutSpec::Observed(observed) if Path::new(&observed.path) == path) {
            checkouts.delete(&checkout.metadata.name).await?;
        }
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
            checkouts: [(
                path,
                Checkout {
                    branch: "main".to_string(),
                    is_main: true,
                    trunk_ahead_behind: None,
                    remote_ahead_behind: None,
                    working_tree: None,
                    last_commit: None,
                    host_name: None,
                    environment_id: None,
                },
            )]
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

    // #768: reconciliation removes only adopted projections without a durable
    // adopted source. Generate authority traversal order and reconcile repeats;
    // exhaust source/observation presence and authorities in each draw so the
    // profile's case limit cannot miss deletion or preservation boundaries.
    #[hegel::test]
    fn adopted_projection_cleanup_preserves_other_authorities(tc: hegel::TestCase) {
        use flotilla_resources::{CheckoutSpec, InputMeta, LifecycleAuthority, ObservedCheckoutSpec};
        let authority_start = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
        let repeats = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let spec = CheckoutSpec::Observed(
                ObservedCheckoutSpec::builder()
                    .r#ref("main".to_string())
                    .path("/work/repo".to_string())
                    .repo_ref(RepositoryKey("repo".to_string()))
                    .host_ref("host".to_string())
                    .is_main(false)
                    .build(),
            );
            let authorities =
                [None, Some(LifecycleAuthority::Observed), Some(LifecycleAuthority::Managed), Some(LifecycleAuthority::Adopted)];
            for offset in 0..authorities.len() {
                let authority = authorities[(authority_start + offset) % authorities.len()];
                for observed_present in [false, true] {
                    for durable_authority in [None, Some(LifecycleAuthority::Managed), Some(LifecycleAuthority::Adopted)] {
                        let durable = ResourceBackend::InMemory(InMemoryBackend::default());
                        let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
                        let mut meta = InputMeta::builder().name("checkout".to_string()).build();
                        if let Some(authority) = authority {
                            meta = meta.with_lifecycle_authority(authority);
                        }
                        let original = if observed_present {
                            Some(observed.using::<ResourceCheckout>("flotilla").create(&meta, &spec).await.expect("observation"))
                        } else {
                            None
                        };
                        if let Some(authority) = durable_authority {
                            let durable_meta =
                                InputMeta::builder().name("checkout".to_string()).build().with_lifecycle_authority(authority);
                            durable.using::<ResourceCheckout>("flotilla").create(&durable_meta, &spec).await.expect("durable checkout");
                        }
                        for _ in 0..repeats {
                            let result = super::reconcile_adopted_checkouts(&durable, &observed, "flotilla").await;
                            if observed_present
                                && durable_authority == Some(LifecycleAuthority::Adopted)
                                && authority != Some(LifecycleAuthority::Adopted)
                            {
                                assert!(result.is_err(), "non-adopted name collisions are refused");
                            } else {
                                result.expect("reconcile");
                            }
                            let stored = observed.using::<ResourceCheckout>("flotilla").list().await.expect("list").items;
                            let should_remain = (observed_present && authority != Some(LifecycleAuthority::Adopted))
                                || durable_authority == Some(LifecycleAuthority::Adopted);
                            assert_eq!(stored.len(), usize::from(should_remain));
                            if observed_present && authority != Some(LifecycleAuthority::Adopted) {
                                let original = original.as_ref().expect("original observation");
                                assert_eq!(stored[0].metadata, original.metadata, "unrelated metadata is untouched");
                                assert_eq!(stored[0].spec, original.spec, "unrelated specs are untouched");
                                assert_eq!(stored[0].status, original.status, "unrelated statuses are untouched");
                            }
                        }
                    }
                }
            }
        });
    }
}
