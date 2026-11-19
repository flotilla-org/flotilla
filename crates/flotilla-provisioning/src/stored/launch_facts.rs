//! EnvironmentLaunchFacts: what one launch of an Environment was given, fixed when it was realised.

use flotilla_resources::EnvironmentProviderDescriptor;
use flotilla_schema::{Frozen, Stored};

/// The facts fixed for one launch of an Environment: kind code `el`, a plain record named and
/// homed as the launch it describes (`name#n` at the provider's host). The Environment
/// controller writes it in the transaction of that launch's Realise edge and nothing updates
/// it; the next launch is a new Environment record and gets a new one. It archives and
/// expunges with its launch.
///
/// Not here: the container's runtime samples, which are metrics.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "EnvironmentLaunchFacts", code = "el")]
pub struct EnvironmentLaunchFacts {
    /// The image reference the launch ran, as the provider resolved it.
    pub image_ref: Option<String>,
    /// The local image id (`sha256:…`) the container was created from.
    pub local_image_id: Option<String>,
    /// The registry digest of that image, when it came from a registry.
    pub registry_digest: Option<String>,
    /// The limits the provider configured; each absent one is unknown, not unlimited.
    #[store(flatten)]
    pub configured_limits: LaunchLimits,
    /// A cloud provider's descriptor of the backing, stored once by content hash.
    #[store(frozen)]
    pub provider_descriptor: Option<Frozen<EnvironmentProviderDescriptor>>,
}

/// [`flotilla_protocol::ConfiguredResourceLimits`] as one launch's columns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Stored)]
pub struct LaunchLimits {
    pub cpus: Option<u64>,
    pub cpu_shares: Option<u64>,
    pub build_jobs: Option<u64>,
    pub linker_threads: Option<u64>,
}
