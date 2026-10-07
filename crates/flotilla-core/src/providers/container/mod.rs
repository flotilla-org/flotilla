//! Host-scoped container and image capabilities. Callers supply credential
//! artifacts per operation; adapters never consult a host-global login.
pub mod docker;
pub mod registry;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_resources::PlacedImageIdentity;

pub use super::environment::ContainerRuntime;
use super::CommandOutput;

/// An admitted private auth directory, never the host's global configuration.
#[derive(Clone, Copy)]
pub struct ImageOperation<'a> {
    pub directory: &'a Path,
    pub context: &'a Path,
}

#[derive(bon::Builder)]
pub struct ImageBuildOptions<'a> {
    pub tag: &'a str,
    pub file: &'a str,
    pub platform: &'a str,
    pub args: &'a BTreeMap<String, String>,
    pub timeout: Duration,
}

/// Immutable local image IDs and registry manifest digests are separate
/// identities. Pull completion must be followed by inspection by the caller.
#[async_trait]
pub trait ImageStore: Send + Sync {
    async fn build(&self, operation: ImageOperation<'_>, options: ImageBuildOptions<'_>) -> Result<CommandOutput, String>;
    async fn inspect(&self, operation: ImageOperation<'_>, reference: &str) -> Result<PlacedImageIdentity, String>;
    async fn inventory(&self, operation: ImageOperation<'_>) -> Result<BTreeSet<String>, String>;
    async fn tag(&self, operation: ImageOperation<'_>, image: &str, reference: &str) -> Result<(), String>;
    async fn pull(&self, operation: ImageOperation<'_>, reference: &str) -> Result<String, String>;
    /// Publish content and return its validated registry manifest digest.
    async fn push(&self, operation: ImageOperation<'_>, reference: &str) -> Result<String, String>;
    async fn save(&self, operation: ImageOperation<'_>, image: &str, archive: &Path) -> Result<(), String>;
    async fn load(&self, operation: ImageOperation<'_>, archive: &Path) -> Result<(), String>;
    async fn remove(&self, operation: ImageOperation<'_>, image: &str) -> Result<(), String>;
    async fn login(&self, operation: ImageOperation<'_>, registry: &str, username: &str, secret: &[u8]) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ProbeNetwork {
    #[default]
    HostDefault,
    Isolated,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ProbeDirectory {
    #[default]
    ImageDefault,
    Temporary,
}

/// Short-lived image verification, independent of vessel provisioning.
#[derive(bon::Builder)]
pub struct ContainerProbe<'a> {
    pub image: &'a str,
    pub name: Option<&'a str>,
    pub command: &'a [String],
    pub cpu: Option<u32>,
    pub memory_bytes: Option<u64>,
    pub pids_limit: Option<u32>,
    #[builder(default)]
    pub network: ProbeNetwork,
    #[builder(default)]
    pub directory: ProbeDirectory,
    pub timeout: Duration,
}
