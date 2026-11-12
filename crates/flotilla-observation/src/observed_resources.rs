use flotilla_protocol::{qualified_path::QualifiedPath, ProviderData};
use flotilla_resources::{
    discovered_checkouts::{DiscoveredCheckout, DiscoveredCheckouts},
    ObservedCheckoutSpec, RepositoryKey,
};
use sha2::{Digest, Sha256};

/// Holds the checkouts discovered for one local repository as this host's discovered checkouts,
/// replacing those it held for it.
///
/// `ProviderData` is the on-demand provider interchange input; its checkout facts stay on this
/// host, in memory, for its readers.
pub fn reconcile_checkouts(
    discovered: &DiscoveredCheckouts,
    repository_key: &RepositoryKey,
    repository_slug: &str,
    providers: &ProviderData,
    host_ref: &str,
) {
    let found = providers
        .checkouts
        .iter()
        .map(|(path, checkout)| DiscoveredCheckout {
            name: observed_checkout_name(repository_key, path),
            repo_slug: repository_slug.to_owned(),
            spec: ObservedCheckoutSpec {
                r#ref: checkout.branch.clone(),
                path: path.path.to_string_lossy().into_owned(),
                repo_ref: repository_key.clone(),
                host_ref: host_ref.to_owned(),
                is_main: checkout.is_main,
            },
        })
        .collect();
    discovered.replace(repository_key, found);
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
    use flotilla_resources::{discovered_checkouts::DiscoveredCheckouts, RepositoryKey};

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

    #[test]
    fn explicit_repository_identity_supports_remote_less_observations() {
        let discovered = DiscoveredCheckouts::default();
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

        reconcile_checkouts(&discovered, &repository_key, "local-repo", &providers, "host-01");

        assert!(matches!(discovered.all().as_slice(), [checkout] if checkout.spec.repo_ref == repository_key));
    }
}
