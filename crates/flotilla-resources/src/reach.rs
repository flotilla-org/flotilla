//! A vessel's reach set (#4299): the destinations derived from what the vessel
//! is given, and those the fleet and project declare. Repository, request and
//! tool or harness levels come later; the composed set is what an egress
//! allowlist is later built from.
use std::collections::BTreeSet;

pub use flotilla_protocol::reach::{ReachDestination, ReachMeans, ReachRoute, ReachWay};

use crate::CredentialConsumer;

/// The destination a URL names, with its scheme's default port: `https`,
/// `http`, `ssh` and `git` URLs, and scp-like `user@host:path` remotes.
/// Local paths and `file:` URLs name none.
pub fn url_destination(url: &str) -> Option<ReachDestination> {
    let (default_port, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (
            match scheme.to_ascii_lowercase().as_str() {
                "https" => 443,
                "http" => 80,
                "ssh" | "git+ssh" | "ssh+git" => 22,
                "git" => 9418,
                _ => return None,
            },
            rest,
        ),
        // `user@host:path`, Git's scp-like syntax, is SSH; a path is local,
        // and so is a Windows drive path such as `C:\repo` or `C:/repo`.
        None => {
            let (authority, path) = url.split_once(':')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
            let drive = authority.len() == 1 && authority.chars().all(|c| c.is_ascii_alphabetic());
            if authority.contains(['/', '\\']) || host.is_empty() || drive || path.starts_with('\\') {
                return None;
            }
            return ReachDestination::new(host, 22).ok();
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, port) = match authority.strip_prefix('[') {
        // `[v6]` or `[v6]:port`
        Some(bracketed) => {
            let (host, after) = bracketed.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => default_port,
    };
    ReachDestination::new(host, port).ok()
}

/// The servers a delivered credential names. A Forgejo credential names its
/// Forge's server, which the caller resolves: `forge_url` is that Forge's
/// HTTPS URL. Harness credentials name none yet (tool and harness reach come
/// later).
pub fn credential_destinations(consumer: &CredentialConsumer, forge_url: Option<&str>) -> Vec<ReachDestination> {
    let https = |host: &str| url_destination(&format!("https://{host}"));
    match consumer {
        CredentialConsumer::Gh | CredentialConsumer::GithubApp { .. } => {
            ["github.com", "api.github.com"].into_iter().filter_map(https).collect()
        }
        CredentialConsumer::Forgejo { .. } => forge_url.and_then(url_destination).into_iter().collect(),
        CredentialConsumer::GitHttpToken { host, .. } => https(host).into_iter().collect(),
        CredentialConsumer::DockerRegistry { registry, .. } => https(registry).into_iter().collect(),
        CredentialConsumer::ReviewBundleStore { endpoint, .. } => url_destination(endpoint).into_iter().collect(),
        CredentialConsumer::Claude | CredentialConsumer::ClaudeOauth { .. } | CredentialConsumer::Codex => Vec::new(),
    }
}

/// Every declared entry valid, or the first that is not.
pub fn validate_declared(reach: &[ReachDestination]) -> Result<(), String> {
    reach.iter().try_for_each(ReachDestination::validate)
}

/// The union of a vessel's derived destinations and the declared levels.
pub fn compose<'a>(
    derived: impl IntoIterator<Item = ReachDestination>,
    declared: impl IntoIterator<Item = &'a [ReachDestination]>,
) -> BTreeSet<ReachDestination> {
    derived.into_iter().chain(declared.into_iter().flatten().cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(host: &str, port: u16) -> ReachDestination {
        ReachDestination::new(host, port).unwrap()
    }

    // #4299: a remote and a delivered credential's server each name the
    // destination the vessel must reach.
    #[test]
    fn destinations_are_derived_from_what_the_vessel_is_given() {
        for (url, expected) in [
            ("https://github.com/flotilla-org/flotilla", Some(at("github.com", 443))),
            ("https://forgejo.lab.example:3000/lab/flotilla.git", Some(at("forgejo.lab.example", 3000))),
            ("http://user:secret@10.0.0.5/repo", Some(at("10.0.0.5", 80))),
            ("ssh://git@git.example:2222/repo.git", Some(at("git.example", 2222))),
            ("git@github.com:flotilla-org/flotilla.git", Some(at("github.com", 22))),
            ("https://[fd00::1]:8443/x", Some(at("fd00::1", 8443))),
            ("/srv/repos/flotilla", None),
            ("file:///srv/repos/flotilla", None),
            ("./relative:path", None),
            (r"C:\repo", None),
            ("C:/repo", None),
        ] {
            assert_eq!(url_destination(url), expected, "{url}");
        }
        let forgejo = CredentialConsumer::Forgejo { forge_ref: "lab".into(), username: "crew".into() };
        assert_eq!(credential_destinations(&forgejo, Some("https://forgejo.lab.example")), vec![at("forgejo.lab.example", 443)]);
        let token = CredentialConsumer::GitHttpToken { host: "git.example:8443".into(), username: "crew".into() };
        assert_eq!(credential_destinations(&token, None), vec![at("git.example", 8443)]);
        let store = CredentialConsumer::ReviewBundleStore {
            endpoint: "http://objects.lab.example:9000".into(),
            bucket: "bundles".into(),
            region: "auto".into(),
            public_base_url: "https://bundles.example".into(),
            allow_http: true,
            virtual_hosted_style: false,
        };
        assert_eq!(credential_destinations(&store, None), vec![at("objects.lab.example", 9000)]);
        assert!(credential_destinations(&CredentialConsumer::Claude, None).is_empty());
    }

    // ADR 0047: records written before #4299 carry no reach and decode with an
    // empty set; an empty set is not written, so the stored shape is unchanged
    // until a vessel has a destination. An advertisement that declares no ways
    // decodes with none and writes none.
    #[test]
    fn previous_generation_records_decode_without_reach() {
        fn without_reach<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(value: T) {
            let json = serde_json::to_value(&value).unwrap();
            assert!(json.get("reach").is_none(), "{json}");
            assert_eq!(serde_json::from_value::<T>(json).unwrap(), value);
        }
        without_reach(crate::VesselRequirement::builder().name("work".into()).crew(Vec::new()).build());
        without_reach(crate::ProjectSpec::builder().display_name("app".into()).build());
        without_reach(crate::FleetDesignationSpec { fleet: None, project: "app".into(), image_cache: None, reach: Vec::new() });
        let capabilities: flotilla_protocol::provider::ProviderCapabilities = serde_json::from_str("{}").unwrap();
        assert!(capabilities.reach_ways.is_empty());
        assert!(serde_json::to_value(&capabilities).unwrap().get("reach_ways").is_none());
    }
}
