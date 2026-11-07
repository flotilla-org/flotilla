use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use flotilla_protocol::{qualified_path::QualifiedPath, ProviderData};
use flotilla_resources::checkout_records::{latest_launches, CheckoutRecord};
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutSpec as ResourceCheckoutSpec, InputMeta, LifecycleAuthority, ObservedCheckoutSpec, RepositoryKey,
    ResourceError, ResourceObject, AUTHORITY_LABEL, REPO_KEY_LABEL, REPO_LABEL,
};
use flotilla_store::ResourceBackend;
use sha2::{Digest, Sha256};

/// Rebuild the query-facing projection of the adopted checkouts from the engine's (#3108): each
/// adopted checkout held here whose latest launch has not ended is published into the ephemeral
/// observed store, and every adopted projection of another name is removed. Callers serialize this
/// with the observed-checkout refresh.
pub async fn project_adopted_checkouts(
    held_here: Vec<CheckoutRecord>,
    observed_backend: &ResourceBackend,
    namespace: &str,
) -> Result<(), ResourceError> {
    let selector = BTreeMap::from([(AUTHORITY_LABEL.to_string(), LifecycleAuthority::Adopted.as_label_value().to_string())]);
    let observed_checkouts = observed_backend.clone().using::<ResourceCheckout>(namespace);
    let adopted = latest_launches(held_here)
        .into_values()
        .filter(|record| matches!(record.spec, ResourceCheckoutSpec::Observed(_)) && !record.ended())
        .map(|record| {
            let mut view = record.read_view();
            view.metadata.namespace = namespace.to_owned();
            (view.metadata.name.clone(), view)
        })
        .collect::<BTreeMap<_, _>>();
    let mut failures = Vec::new();
    for (name, view) in &adopted {
        if let Err(error) = project_adopted_checkout_with(&observed_checkouts, view).await {
            failures.push(format!("{name}: {error}"));
        }
    }
    for checkout in observed_checkouts.list_matching_labels(&selector).await?.items {
        let name = &checkout.metadata.name;
        if adopted.contains_key(name) {
            continue;
        }
        match observed_checkouts.delete(name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ResourceError::other(format!("failed to project adopted checkouts: {}", failures.join("; "))))
    }
}

async fn project_adopted_checkout_with(
    checkouts: &flotilla_store::TypedResolver<ResourceCheckout>,
    adopted: &ResourceObject<ResourceCheckout>,
) -> Result<(), ResourceError> {
    let meta = InputMeta::from(&adopted.metadata);
    let projected = match checkouts.create(&meta, &adopted.spec).await {
        Ok(created) => created,
        Err(ResourceError::Conflict { .. }) => {
            let existing = checkouts.get(&adopted.metadata.name).await?;
            if existing.metadata.lifecycle_authority()? != Some(LifecycleAuthority::Adopted) {
                return Err(ResourceError::invalid(format!(
                    "checkout {} already exists in the observed store but is not adopted",
                    adopted.metadata.name
                )));
            }
            if existing.spec == adopted.spec && existing.metadata.labels == meta.labels && existing.status == adopted.status {
                return Ok(());
            }
            checkouts.update(&meta, &existing.metadata.resource_version, &adopted.spec).await?
        }
        Err(error) => return Err(error),
    };
    if let Some(status) = &adopted.status {
        checkouts.update_status(&adopted.metadata.name, &projected.metadata.resource_version, status).await?;
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
    use flotilla_resources::{Checkout as ResourceCheckout, RepositoryKey};
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;

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
        let backend = ResourceBackend::new(InMemoryBackend::observed());
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

    // #768, on the engine's records (#3108): the projection removes only adopted
    // projections with no live adopted launch held here. Generate authority
    // traversal order and projection repeats; exhaust launch/observation presence
    // and authorities in each draw so the profile's case limit cannot miss
    // deletion or preservation boundaries.
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
                    let launches = [
                        None,
                        Some(flotilla_protocol::LifecycleReading::Ready { identity: "/work/repo".into() }),
                        Some(flotilla_protocol::LifecycleReading::Released),
                    ];
                    for launch in launches {
                        let live = launch.as_ref().is_some_and(|reading| !matches!(reading, flotilla_protocol::LifecycleReading::Released));
                        let held_here = launch
                            .iter()
                            .map(|reading| flotilla_resources::checkout_records::CheckoutRecord {
                                name: "checkout#1".into(),
                                home: "host".into(),
                                spec: spec.clone(),
                                holder: None,
                                requested_at: chrono::DateTime::UNIX_EPOCH,
                                reading: reading.clone(),
                                degraded: None,
                                integration: None,
                            })
                            .collect::<Vec<_>>();
                        let observed = ResourceBackend::new(InMemoryBackend::observed());
                        let mut meta = InputMeta::builder().name("checkout".to_string()).build();
                        if let Some(authority) = authority {
                            meta = meta.with_lifecycle_authority(authority);
                        }
                        let original = if observed_present {
                            Some(observed.using::<ResourceCheckout>("flotilla").create(&meta, &spec).await.expect("observation"))
                        } else {
                            None
                        };
                        for _ in 0..repeats {
                            let result = super::project_adopted_checkouts(held_here.clone(), &observed, "flotilla").await;
                            if observed_present && live && authority != Some(LifecycleAuthority::Adopted) {
                                assert!(result.is_err(), "non-adopted name collisions are refused");
                            } else {
                                result.expect("reconcile");
                            }
                            let stored = observed.using::<ResourceCheckout>("flotilla").list().await.expect("list").items;
                            let should_remain = (observed_present && authority != Some(LifecycleAuthority::Adopted)) || live;
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
