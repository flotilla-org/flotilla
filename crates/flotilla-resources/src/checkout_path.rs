//! Shared checkout path selection from durable and observed facts.
use sha2::{Digest, Sha256};

use crate::{convoy::engine::launch_slug, CheckoutSpec, CheckoutStatus, Convoy, ResourceObject, ACTUATOR_SOURCE_ROOT_ANNOTATION};

/// The name of a managed checkout for `vessel_name`, scoped by
/// [`checkout_placement_scope`] when it has one.
pub fn checkout_name(vessel_name: &str, workspace_slug: &str, multi_repository: bool, placement_scope: Option<&str>) -> String {
    let repository_suffix = if multi_repository { format!("-{workspace_slug}") } else { String::new() };
    let placement_suffix = placement_scope.map(|scope| format!("-{scope}")).unwrap_or_default();
    format!("checkout-{vessel_name}{repository_suffix}{placement_suffix}")
}

/// What a vessel launch's own resources are named after (a clone made in the vessel, its
/// environment, its crew's sessions): the launch's path-safe slug, so the crew and the import
/// name them alike.
pub fn vessel_launch_slug(launch: &str) -> String {
    launch_slug(launch)
}

/// Managed checkout `old`, which [`checkout_name`] named for `old_owner` (an old store's convoy or
/// vessel), as named for `owner`, the launch that holds it on the engine: the rest of the name
/// (repository and placement scope) is kept. `None` when `old` was not named for `old_owner`.
pub fn checkout_renamed(old: &str, old_owner: &str, owner: &str) -> Option<String> {
    let rest = old.strip_prefix("checkout-")?.strip_prefix(old_owner)?;
    (rest.is_empty() || rest.starts_with('-')).then(|| format!("checkout-{owner}{rest}"))
}

/// The scope of `convoy`'s managed checkouts on `host_ref`: it hashes the convoy's creation
/// timestamp's RFC 3339 form, so a convoy keeps its checkout names only while that stamp is
/// served exactly.
pub fn checkout_placement_scope(convoy: &ResourceObject<Convoy>, host_ref: Option<&str>) -> Option<String> {
    let host_ref = host_ref?;
    let mut hash = Sha256::new();
    hash.update(b"managed-checkout-placement-v1\0");
    hash.update(host_ref.as_bytes());
    hash.update([0]);
    hash.update(convoy.metadata.creation_timestamp.to_rfc3339().as_bytes());
    hash.update([0]);
    if let Some(origin_root) = convoy.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION) {
        hash.update(origin_root.as_bytes());
    }
    let digest = format!("{:x}", hash.finalize());
    Some(digest[..12].to_string())
}

/// Prefer the published path, then a declared target, then the observed path.
pub fn checkout_path_from_status_and_spec<'a>(status: Option<&'a CheckoutStatus>, spec: &'a CheckoutSpec) -> Option<&'a str> {
    status.as_ref().and_then(|status| status.path.as_deref()).or_else(|| spec.target_path()).or(match spec {
        CheckoutSpec::Observed(spec) => Some(spec.path.as_str()),
        _ => None,
    })
}

/// Select the checkout path from its published status and declared specification.
pub fn checkout_path(checkout: &crate::ResourceObject<crate::Checkout>) -> Option<&str> {
    checkout_path_from_status_and_spec(checkout.status.as_ref(), &checkout.spec)
}

/// The filesystem component used for both branch refs and convoy names.
/// Provisioning and cleanup must share this normalization rule.
pub fn checkout_path_component(branch: &str) -> String {
    let normalized = branch
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') { character } else { '-' })
        .collect::<String>();
    let normalized = normalized.trim_matches(['-', '.']).to_string();
    if normalized.is_empty() {
        "work".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Branches and convoy names use the same safe component, including empty
    // names, separators, Unicode and names made entirely of trimmed characters.
    #[rstest::rstest]
    #[case("feature/topic", "feature-topic")]
    #[case("..topic_1-..", "topic_1")]
    #[case("a.b-c_d9", "a.b-c_d9")]
    #[case("", "work")]
    #[case("/..-", "work")]
    #[case("é/topic", "topic")]
    fn checkout_component_normalizes_names(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(checkout_path_component(name), expected);
    }

    // Glue: the resource wrapper delegates to the shared selection policy,
    // which prefers published status over the observed specification.
    #[test]
    fn checkout_resource_uses_published_path() {
        let checkout = crate::ResourceObject::<crate::Checkout> {
            metadata: crate::ObjectMeta {
                name: "checkout".into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: Default::default(),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: chrono::Utc::now(),
                merge: None,
            },
            spec: CheckoutSpec::Observed(crate::ObservedCheckoutSpec {
                r#ref: "topic".into(),
                path: "/observed".into(),
                repo_ref: crate::RepositoryKey("repo".into()),
                host_ref: "host".into(),
                is_main: false,
            }),
            status: Some(CheckoutStatus::builder().phase(crate::CheckoutPhase::Ready).path("/published".into()).build()),
        };
        assert_eq!(checkout_path(&checkout), Some("/published"));
        assert_eq!(checkout_path(&crate::ResourceObject { status: None, ..checkout }), Some("/observed"));
    }
}
