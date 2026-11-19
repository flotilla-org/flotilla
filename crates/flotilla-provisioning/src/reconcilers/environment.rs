//! The environment provider port the Environment controller drives
//! ([`crate::engine::environment_actuator`]), and what a provision reports.

use async_trait::async_trait;
use flotilla_environment::environment::EnvironmentProvisionError;
use flotilla_protocol::ConfiguredResourceLimits;

#[async_trait]
pub trait EnvironmentRuntime: Send + Sync {
    /// Adopt or provision through the provider addressed by the full spec.
    async fn provision_environment(
        &self,
        name: &str,
        spec: &flotilla_resources::EnvironmentSpec,
    ) -> Result<EnvironmentProvisioning, EnvironmentProvisionError>;
    async fn destroy(&self, environment_ref: &str, backing_id: &str) -> Result<(), String>;
    /// Recover a provider backing whose identity was never committed to status.
    async fn destroy_unrecorded(&self, environment_ref: &str) -> Result<(), String> {
        self.cleanup(environment_ref).await
    }
    async fn cleanup(&self, _environment_ref: &str) -> Result<(), String> {
        Ok(())
    }
}

/// What a provision reports, by the kind of backing it made: the identity the launch is realised
/// under, and the facts it fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentProvisioning {
    /// A Docker or host backing.
    Local(LocalProvisioning),
    /// A cloud sandbox backing, which records every fact below or fails provision.
    Cloud(CloudProvisioning),
}

/// What a Docker or host backing reports; each fact is recorded as reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalProvisioning {
    /// Docker's per-crew limits; host backings set none.
    pub configured_limits: Option<ConfiguredResourceLimits>,
    /// The backing's opaque identity; a host backing has none, and is realised under its root.
    pub backing_id: Option<String>,
    /// The image the backing runs, when it reports the image's digest.
    pub image_ref: Option<String>,
    /// The content digest of that image.
    pub local_image_id: Option<String>,
    /// The registry manifest identity, when the image was pulled by digest.
    pub registry_digest: Option<String>,
}

/// Every fact a cloud backing reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudProvisioning {
    /// The backing's opaque provider identity, used again for teardown.
    pub backing_id: String,
    /// The image the backing runs, pinned by digest: a cloud provider refuses
    /// an unpinned image at prepare.
    pub image_ref: String,
    /// The provider's immutable facts about this backing.
    pub provider_descriptor: Box<flotilla_resources::EnvironmentProviderDescriptor>,
}
