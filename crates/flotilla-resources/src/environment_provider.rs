//! A host's environment provider settings (#3824 layer 4, ADR 0058): how one provider instance on
//! one host realises a vessel. The 2026-10-26 ruling moves the realisation settings that only a
//! PlacementPolicy carried (Docker memory policy, pull policy, checkout strategy; host-direct
//! checkout) onto the provider, unchanged in meaning; a FulfilmentKind keeps host, pool, grants,
//! cost class and image.

use std::collections::BTreeMap;

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{
    CloudSandboxCheckout, CloudSandboxSize, DockerCheckoutStrategy, DockerImagePullPolicy, EnvironmentMemoryPolicy,
    HostDirectPlacementPolicyCheckout,
};

/// One provider instance's settings on one host, named `<host_ref>.<instance or realisation>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProviderSpec {
    /// The host whose provider these settings are for; empty for a provider bound to no host.
    pub host_ref: String,
    /// The provider instance; omitted is the host's one provider of the realisation, as admission
    /// resolves it today.
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
    /// The realisation's name, as the settings' record name uses it when no instance is named.
    pub const fn realisation(&self) -> &'static str {
        match self {
            Self::DockerPerVessel { .. } => "docker_per_vessel",
            Self::HostDirect { .. } => "host_direct",
            Self::CloudSandboxPerVessel { .. } => "cloud_sandbox_per_vessel",
        }
    }
}

impl EnvironmentProviderSpec {
    /// The settings' record name: `<host_ref>.<instance>`, else `<host_ref>.<realisation>`; the
    /// host part is left out for a provider bound to no host.
    pub fn record_name(&self) -> String {
        let key = self.instance.as_deref().unwrap_or(self.settings.realisation());
        if self.host_ref.is_empty() {
            key.to_owned()
        } else {
            format!("{}.{key}", self.host_ref)
        }
    }
}
