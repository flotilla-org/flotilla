//! Session capability contract: what a credential runtime reports about
//! delivered credentials and endpoints. Credential material never appears
//! here; cards contain only scope and effective permissions.
use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct CredentialCapability {
    pub name: String,
    pub repositories: Vec<String>,
    pub permissions: Option<BTreeMap<String, String>>,
}

/// Reports delivered credentials, rather than desired grants or token contents.
/// A refresh that fails must leave this observation at the previous delivery.
#[async_trait]
pub trait SessionCapabilitySource: Send + Sync {
    async fn credentials(&self, environment: &str, references: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String>;
    /// Reports environment-wide endpoints plus this session's delivered endpoints.
    async fn endpoints(&self, _environment: &str, _session: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::new())
    }
}

/// Project explicitly named endpoint variables without URL authentication,
/// queries or fragments. Proxy paths are stripped; Forgejo API routing paths
/// are retained. Unrecognized environment entries are never a card.
pub fn endpoint_for_env(key: &str, value: &str) -> Option<(String, String)> {
    let destination = match key {
        "FLOTILLA_DAEMON_SOCKET" => "Flotilla daemon",
        "FORGEJO_API_URL" => "Forgejo API",
        "DISPLAY" | "WAYLAND_DISPLAY" => "GUI display",
        _ => match key.to_ascii_uppercase().as_str() {
            "HTTP_PROXY" => "HTTP egress proxy",
            "HTTPS_PROXY" => "HTTPS egress proxy",
            "ALL_PROXY" => "Egress proxy",
            _ => return None,
        },
    };
    let address = if destination.contains("proxy") || destination == "Forgejo API" {
        let mut address = url::Url::parse(value).ok()?;
        address.host_str()?;
        address.set_username("").ok()?;
        address.set_password(None).ok()?;
        if destination.contains("proxy") {
            address.set_path("");
        }
        address.set_query(None);
        address.set_fragment(None);
        address.to_string()
    } else {
        value.to_string()
    };
    Some((destination.to_string(), address))
}

pub fn credential_card(credentials: &[CredentialCapability]) -> String {
    let mut text = String::new();
    for credential in credentials {
        text.push_str(&format!("- Credential `{}`: repositories [{}]; permissions: ", credential.name, credential.repositories.join(", ")));
        match &credential.permissions {
            Some(permissions) => {
                text.push_str(&permissions.iter().map(|(name, level)| format!("{name}: {level}")).collect::<Vec<_>>().join(", "))
            }
            None => text.push_str("not reported by the credential adapter"),
        }
        text.push('\n');
    }
    let writable = credentials
        .iter()
        .filter(|credential| {
            credential.permissions.as_ref().is_some_and(|permissions| {
                permissions.get("workflows").is_some_and(|level| level == "write")
                    && permissions.get("contents").is_some_and(|level| level == "write")
            })
        })
        .flat_map(|credential| credential.repositories.iter().cloned())
        .collect::<BTreeSet<_>>();
    if writable.is_empty() && credentials.iter().any(|credential| credential.permissions.is_none()) {
        text.push_str("Workflow push permissions are unreported for this delivery. Run `flotilla crew capabilities` after permissions are confirmed.\n");
    } else if writable.is_empty() {
        text.push_str("You can supply `.github/workflows` changes as a fenced diff in the PR description under 'Operator-applied workflow change', keeping them out of your commits; the operator applies it.\n");
    } else {
        text.push_str(&format!("You can push `.github/workflows` changes in [{}].\n", writable.into_iter().collect::<Vec<_>>().join(", ")));
        if credentials.iter().any(|credential| credential.permissions.is_none()) {
            text.push_str("Workflow push permissions remain unreported for other credentials.\n");
        } else {
            text.push_str(
                "For other repositories in scope, supply workflow changes as an operator-applied fenced diff in the PR description.\n",
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proxy endpoints retain the destination, never URL authentication or
    // credential-bearing paths, queries and fragments.
    #[hegel::test]
    fn proxy_endpoints_drop_authentication(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let key = ["HTTP_PROXY", "https_proxy", "ALL_PROXY"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let port = tc.draw(gs::integers::<u16>());
        let original = format!("https://private-user:private-password@127.0.0.1:{port}/private-path?private-query=value#private-fragment");
        let (_, endpoint) = endpoint_for_env(key, &original).expect("valid proxy endpoint");
        let public = url::Url::parse(&endpoint).expect("public URL");
        assert_eq!(public.host_str(), Some("127.0.0.1"));
        assert_eq!(public.port_or_known_default(), Some(port));
        assert!(public.username().is_empty());
        assert!(public.password().is_none());
        assert!(public.query().is_none());
        assert!(public.fragment().is_none());
        assert!(!endpoint.contains("private"));
        assert!(endpoint_for_env("GITHUB_TOKEN", "private-token").is_none());
        assert!(endpoint_for_env(key, "not a URL").is_none());
    }

    // Unreported permissions remain unknown, including alongside known read permissions.
    #[test]
    fn unknown_permissions_do_not_claim_workflow_authority() {
        let unknown = CredentialCapability::builder().name("unknown".into()).repositories(vec!["repo".into()]).build();
        let known = CredentialCapability::builder()
            .name("known".into())
            .repositories(vec!["other".into()])
            .permissions(BTreeMap::from([("contents".into(), "read".into())]))
            .build();
        for credentials in [vec![unknown.clone()], vec![unknown, known]] {
            let card = credential_card(&credentials);
            assert!(card.contains("Workflow push permissions are unreported"));
            assert!(!card.contains("You can push"));
            assert!(!card.contains("the operator applies it"));
        }
    }

    // Workflow push guidance follows effective permission, independently of role.
    #[hegel::test]
    fn workflow_guidance_follows_effective_permission(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Covers missing, read, write, empty and additional unrelated grants.
        let workflow = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let mut permissions = BTreeMap::new();
        let contents_write = tc.draw(gs::booleans());
        if contents_write {
            permissions.insert("contents".into(), "write".into());
        }
        if workflow > 0 {
            permissions.insert("workflows".into(), ["", "read", "write"][workflow].into());
        }
        let credential = CredentialCapability::builder()
            .name("github".into())
            .repositories(vec!["flotilla-org/flotilla".into()])
            .permissions(permissions)
            .build();
        let text = credential_card(&[credential]);
        let can_push = workflow == 2 && contents_write;
        assert_eq!(text.contains("You can push `.github/workflows`"), can_push);
        assert_eq!(text.contains("the operator applies it"), !can_push);
        assert!(text.contains("flotilla-org/flotilla"));
    }
}
