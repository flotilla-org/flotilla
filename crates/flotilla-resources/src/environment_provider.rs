//! A host's environment provider settings (#3824 layer 4, ADR 0058): how one provider instance on
//! one host realises a vessel. The 2026-10-26 ruling moves the realisation settings that only a
//! PlacementPolicy carried (Docker memory policy, pull policy, checkout strategy; host-direct
//! checkout) onto the provider, unchanged in meaning; a FulfilmentKind keeps host, pool, grants,
//! cost class and image.

use std::collections::BTreeMap;

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, NoStatusPatch, ReplicationClass};
use crate::{
    CloudSandboxCheckout, CloudSandboxSize, DockerCheckoutStrategy, DockerImagePullPolicy, EnvironmentMemoryPolicy,
    HostDirectPlacementPolicyCheckout, PlacementPolicySpec,
};

// Authored on the host whose provider it sets (seeded by its daemon, or declared by a manifest),
// and replicated read-only, like a FulfilmentKind.
define_resource!(
    EnvironmentProvider,
    "environmentproviders",
    EnvironmentProviderSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &["metadata.name", "spec.host_ref"]
);

/// The settings of a host's provider of one realisation, named `<host_ref>.<realisation>`: one per
/// realisation per host, as admission resolves a policy's provider today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProviderSpec {
    /// The host whose provider these settings are for; empty for a provider bound to no host.
    pub host_ref: String,
    /// The provider instance, when one is named; omitted is the host's one provider of the
    /// realisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(flatten)]
    pub settings: ProviderSettings,
}

/// What a provider of each realisation is set to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(tag = "realisation", rename_all = "snake_case")]
pub enum ProviderSettings {
    #[store(code = "dk")]
    DockerPerVessel {
        #[serde(default)]
        pull_policy: DockerImagePullPolicy,
        #[serde(default)]
        #[store(flatten)]
        memory_policy: EnvironmentMemoryPolicy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<String>,
        #[serde(default)]
        #[store(child)]
        env: BTreeMap<String, String>,
        checkout: DockerCheckoutStrategy,
    },
    #[store(code = "hd")]
    HostDirect {
        #[serde(rename = "checkout")]
        host_checkout: HostDirectPlacementPolicyCheckout,
    },
    #[store(code = "cs")]
    CloudSandboxPerVessel {
        #[store(flatten)]
        size: CloudSandboxSize,
        #[serde(rename = "checkout")]
        sandbox_checkout: CloudSandboxCheckout,
        #[serde(default)]
        #[store(child)]
        env: BTreeMap<String, String>,
    },
}

impl ProviderSettings {
    /// The settings a host's provider of `realisation` runs with when nothing declares its own:
    /// those the daemon seeded its `docker-on-*` and `host-direct-*` policies with, now built into
    /// the code (the 2026-10-26 ruling on built-ins). A cloud sandbox has none: its size is
    /// always declared.
    pub fn builtin(realisation: &str) -> Option<Self> {
        match realisation {
            "docker_per_vessel" => Some(Self::DockerPerVessel {
                pull_policy: DockerImagePullPolicy::default(),
                memory_policy: EnvironmentMemoryPolicy::default(),
                default_cwd: Some("/workspace".into()),
                env: BTreeMap::new(),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".into() },
            }),
            "host_direct" => Some(Self::HostDirect { host_checkout: HostDirectPlacementPolicyCheckout::Worktree }),
            _ => None,
        }
    }

    /// The realisation's name, as the settings' record name uses it.
    pub const fn realisation(&self) -> &'static str {
        match self {
            Self::DockerPerVessel { .. } => "docker_per_vessel",
            Self::HostDirect { .. } => "host_direct",
            Self::CloudSandboxPerVessel { .. } => "cloud_sandbox_per_vessel",
        }
    }
}

impl EnvironmentProviderSpec {
    /// The settings' record name: `<host_ref>.<realisation>`; the host part is left out for a
    /// provider bound to no host.
    pub fn record_name(&self) -> String {
        let key = self.settings.realisation();
        if self.host_ref.is_empty() {
            key.to_owned()
        } else {
            format!("{}.{key}", self.host_ref)
        }
    }
}

impl EnvironmentProviderSpec {
    /// The settings `policy` carries for its host's provider: what a host realises a vessel with
    /// until it has a record of its own, and what the import makes of a live policy. The policy's
    /// other provider constraints stay with its provider matching; the import refuses them.
    // #3824 layer 4b-ii removes the policy as a source, with PlacementPolicy.
    pub fn from_policy(policy: &PlacementPolicySpec) -> Result<Self, String> {
        let requirements = policy.provider_requirements.clone().unwrap_or_default();
        let (host_ref, settings) = match (&policy.docker_per_vessel, &policy.host_direct, &policy.cloud_sandbox_per_vessel) {
            (Some(docker), None, None) => (
                docker.host_ref.clone(),
                ProviderSettings::DockerPerVessel {
                    pull_policy: docker.pull_policy,
                    memory_policy: docker.memory_policy.clone(),
                    default_cwd: docker.default_cwd.clone(),
                    env: docker.env.clone(),
                    checkout: docker.checkout.clone(),
                },
            ),
            (None, Some(host), None) => (host.host_ref.clone(), ProviderSettings::HostDirect { host_checkout: host.checkout.clone() }),
            (None, None, Some(cloud)) => (
                String::new(),
                ProviderSettings::CloudSandboxPerVessel {
                    size: cloud.size.clone(),
                    sandbox_checkout: cloud.checkout,
                    env: cloud.env.clone(),
                },
            ),
            _ => return Err("it defines no single realisation".into()),
        };
        Ok(Self { host_ref, instance: requirements.instance, settings })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    // Behaviour (#3824 layer 4b-i): this repository's declared provider settings decode, and each
    // is named by its host and realisation, the name every reader looks it up by.
    #[test]
    fn this_repositorys_provider_settings_are_named_by_their_host_and_realisation() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.flotilla");
        let mut found = 0;
        for entry in std::fs::read_dir(&root).expect(".flotilla") {
            let path = entry.expect("entry").path();
            if !path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("environment-provider.")) {
                continue;
            }
            let document: serde_json::Value = serde_yml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("yaml");
            let spec: EnvironmentProviderSpec = serde_json::from_value(document["spec"].clone()).expect("spec decodes");
            assert_eq!(document["kind"], "EnvironmentProvider", "{}", path.display());
            assert_eq!(document["metadata"]["name"], spec.record_name(), "{}", path.display());
            found += 1;
        }
        assert_eq!(found, 3, "kiwi's, feta's and udder's Docker providers");
    }
}
