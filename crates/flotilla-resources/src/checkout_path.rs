//! Shared checkout path selection from durable and observed facts.
use crate::{CheckoutSpec, CheckoutStatus};

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
