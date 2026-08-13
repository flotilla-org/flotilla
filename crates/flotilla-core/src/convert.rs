//! Conversion functions from core types to protocol types.
//!
//! This module is the serialization boundary between the rich in-process
//! core types and the flat, serde-friendly protocol types.

use std::collections::HashMap;

use flotilla_protocol::{DiscoveryEntry, DiscoveryFact, HostProviderStatus, ToolInventory, UnmetRequirementInfo};

use crate::providers::discovery::{EnvironmentAssertion, EnvironmentBag, HostPlatform, UnmetRequirement};

pub fn assertion_to_discovery_entry(assertion: &EnvironmentAssertion) -> DiscoveryEntry {
    let mut detail = HashMap::new();
    let kind = match assertion {
        EnvironmentAssertion::BinaryAvailable { name, path, version } => {
            detail.insert("name".into(), name.clone());
            detail.insert("path".into(), path.as_path().display().to_string());
            if let Some(v) = version {
                detail.insert("version".into(), v.clone());
            }
            "binary_available"
        }
        EnvironmentAssertion::EnvVarSet { key, .. } => {
            detail.insert("key".into(), key.clone());
            detail.insert("value".into(), "<set>".into());
            "env_var_set"
        }
        EnvironmentAssertion::VcsCheckoutDetected { root, kind, is_main_checkout } => {
            detail.insert("root".into(), root.as_path().display().to_string());
            detail.insert("kind".into(), format!("{kind:?}"));
            detail.insert("is_main_checkout".into(), is_main_checkout.to_string());
            "vcs_checkout_detected"
        }
        EnvironmentAssertion::RemoteHost { platform, owner, repo, remote_name } => {
            detail.insert("platform".into(), format!("{platform:?}"));
            detail.insert("owner".into(), owner.clone());
            detail.insert("repo".into(), repo.clone());
            detail.insert("remote_name".into(), remote_name.clone());
            "remote_host"
        }
        EnvironmentAssertion::AuthFileExists { provider, path } => {
            detail.insert("provider".into(), provider.clone());
            detail.insert("path".into(), path.as_path().display().to_string());
            "auth_file_exists"
        }
        EnvironmentAssertion::SocketAvailable { name, path } => {
            detail.insert("name".into(), name.clone());
            detail.insert("path".into(), path.as_path().display().to_string());
            "socket_available"
        }
    };
    DiscoveryEntry { kind: kind.into(), detail }
}

pub fn health_to_proto(health: &HashMap<(&'static str, String), bool>) -> HashMap<String, HashMap<String, bool>> {
    let mut nested: HashMap<String, HashMap<String, bool>> = HashMap::new();
    for ((category, provider), &healthy) in health {
        nested.entry(category.to_string()).or_default().insert(provider.clone(), healthy);
    }
    nested
}

pub fn inventory_from_bag(bag: &EnvironmentBag) -> ToolInventory {
    let mut inventory = ToolInventory::default();

    for assertion in bag.assertions() {
        match assertion {
            EnvironmentAssertion::BinaryAvailable { name, path, version } => {
                let mut detail = vec![("path".into(), path.as_path().display().to_string())];
                if let Some(version) = version {
                    detail.push(("version".into(), version.clone()));
                }
                inventory.binaries.push(DiscoveryFact { name: name.clone(), detail });
            }
            EnvironmentAssertion::SocketAvailable { name, path } => {
                let detail = vec![("path".into(), path.as_path().display().to_string())];
                inventory.sockets.push(DiscoveryFact { name: name.clone(), detail });
            }
            EnvironmentAssertion::AuthFileExists { provider, path } => {
                let detail = vec![("path".into(), path.as_path().display().to_string())];
                inventory.auth.push(DiscoveryFact { name: provider.clone(), detail });
            }
            EnvironmentAssertion::EnvVarSet { key, .. } => {
                let detail = vec![("value".into(), "<set>".into())];
                inventory.env_vars.push(DiscoveryFact { name: key.clone(), detail });
            }
            EnvironmentAssertion::VcsCheckoutDetected { .. } | EnvironmentAssertion::RemoteHost { .. } => {}
        }
    }

    inventory.binaries.sort_by(|a, b| a.name.cmp(&b.name));
    inventory.sockets.sort_by(|a, b| a.name.cmp(&b.name));
    inventory.auth.sort_by(|a, b| a.name.cmp(&b.name));
    inventory.env_vars.sort_by(|a, b| a.name.cmp(&b.name));

    inventory
}

pub fn unmet_requirement_to_proto(factory: &str, requirement: &UnmetRequirement) -> UnmetRequirementInfo {
    let (kind, value) = match requirement {
        UnmetRequirement::MissingBinary(binary) => ("missing_binary", Some(binary.clone())),
        UnmetRequirement::MissingEnvVar(key) => ("missing_env_var", Some(key.clone())),
        UnmetRequirement::MissingAuth(provider) => ("missing_auth", Some(provider.clone())),
        UnmetRequirement::MissingConfig(key) => ("missing_config", Some(key.clone())),
        UnmetRequirement::MissingRemoteHost(platform) => ("missing_remote_host", Some(host_platform_name(*platform).to_string())),
        UnmetRequirement::NoVcsCheckout => ("no_vcs_checkout", None),
        UnmetRequirement::UnknownProviderPreference { key, .. } => ("unknown_provider_preference", Some(key.clone())),
    };

    UnmetRequirementInfo { factory: factory.to_string(), kind: kind.to_string(), value }
}

fn host_platform_name(platform: HostPlatform) -> &'static str {
    match platform {
        HostPlatform::GitHub => "github",
        HostPlatform::GitLab => "gitlab",
    }
}

pub fn provider_health_to_host_statuses(health: &HashMap<(&'static str, String), bool>) -> Vec<HostProviderStatus> {
    let mut statuses: Vec<HostProviderStatus> = health
        .iter()
        .map(|((category, name), healthy)| HostProviderStatus {
            category: category.to_string(),
            name: name.clone(),
            // TODO: derives implementation from display name by lowercasing. The
            // primary path (provider_statuses_from_registries) uses the actual registry
            // key. These must agree — currently safe because all providers use lowercase
            // keys, but will break if a display name diverges from its key.
            implementation: name.to_lowercase(),
            healthy: *healthy,
            disabled_reason: None,
        })
        .collect();
    statuses.sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.name.cmp(&b.name)));
    statuses
}
