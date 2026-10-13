//! Image capabilities are independent of environment lifecycle and executors.
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use flotilla_resources::{ImageBuildFailure, ImageBuildFailureClass, ImageLayerProbe, ImageLayerSpec, PlacedImageIdentity};

use super::RegistryAuth;

#[async_trait]
pub trait LocalImageCache: Send + Sync {
    async fn inspect(&self, digest: &str) -> Result<Option<PlacedImageIdentity>, String>;
    async fn has(&self, digest: &str) -> Result<bool, String> {
        Ok(self.inspect(digest).await?.is_some())
    }
    /// Read-only observation of a reference, including mutable baseline tags.
    /// This does not admit a tag for provisioning or relax digest-only inspect.
    /// The default supports digest references only, through `inspect`; adapters
    /// must override it to observe mutable baseline tags. Unsupported or malformed
    /// observations return an error, never an affirmative availability claim.
    async fn inspect_reference(&self, reference: &str) -> Result<Option<PlacedImageIdentity>, String> {
        self.inspect(reference).await
    }
    /// Check registry availability without pulling. The adapter uses the
    /// host tool's configured registry credentials for candidate validation.
    async fn registry_available(&self, _reference: &str) -> Result<(), String> {
        Err("image cache does not support registry availability checks".into())
    }
    async fn pull(&self, reference: &str, auth: Option<&RegistryAuth>) -> Result<(), String>;
    async fn inventory(&self) -> Result<BTreeSet<String>, String>;
    async fn remove(&self, digest: &str) -> Result<(), String>;
}

/// Builds one composed image identity for one architecture and verifies every
/// layer's probe on the result (ADR 0062). Implementations are an owned
/// BuildKit builder or a remote/managed builder; callers see only the digest.
#[async_trait]
pub trait ImageBuildService: Send + Sync {
    /// Build, deliver and probe. `log` receives the redacted build and probe
    /// output for the build-log artifact, whatever the outcome.
    async fn build(&self, request: ImageBuildRequest<'_>, log: &mut String) -> Result<BuiltImage, FailedImageBuild>;
}

/// One build of one identity × architecture.
#[derive(bon::Builder)]
pub struct ImageBuildRequest<'a> {
    /// [`flotilla_resources::image_identity`] of `layers`.
    pub identity: &'a str,
    /// Canonical architecture: `amd64` or `arm64`.
    pub architecture: &'a str,
    /// Ordered composition, base first.
    pub layers: &'a [ImageLayerSpec],
    pub delivery: ImageDeliveryTarget,
    /// Granted build secrets by name. Only the names the layers declare are
    /// passed to the builder; a declared name missing here refuses the build.
    #[builder(default)]
    pub secrets: BTreeMap<String, SecretFile>,
    /// Shell script that runs `"$@"` under the image's preludes (the agent
    /// crate's `with_preludes("exec \"$@\"")`). Each layer probe runs as
    /// `sh -c <wrapper> flotilla-layer-probe sh -c <probe>`.
    pub probe_wrapper: &'a str,
    /// CPUs granted to each probe container (the build's CPU reservation).
    pub probe_cpus: u32,
}

/// Where the built image goes.
pub enum ImageDeliveryTarget {
    /// Push to `repository` with a registry build cache at `cache_repository`.
    /// `auth` is the builder's push credential, admitted for `repository`.
    Push { repository: String, cache_repository: String, auth: RegistryAuth },
    /// Single-host mode: load into the builder host's local image store.
    Load,
}

impl fmt::Debug for ImageDeliveryTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Push { repository, cache_repository, .. } => {
                f.debug_struct("Push").field("repository", repository).field("cache_repository", cache_repository).finish_non_exhaustive()
            }
            Self::Load => f.write_str("Load"),
        }
    }
}

/// A build secret delivered as a file. The optional owner keeps the material
/// alive for the operation and cleans it up on drop.
pub struct SecretFile {
    path: PathBuf,
    _owner: Option<Box<dyn Any + Send + Sync>>,
}

impl SecretFile {
    pub fn new(path: PathBuf, owner: impl Any + Send + Sync) -> Self {
        Self { path, _owner: Some(Box::new(owner)) }
    }
    /// A file whose lifetime the caller manages.
    pub fn at(path: PathBuf) -> Self {
        Self { path, _owner: None }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretFile").field("path", &self.path).finish_non_exhaustive()
    }
}

/// A built and verified image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltImage {
    /// Push: the pushed manifest, `<repository>@sha256:…`.
    /// Load: the local `sha256:…` image ID, not a registry digest despite
    /// the field's name. It names the image only in this host's Docker store
    /// (single-host delivery) and is not pullable from anywhere.
    pub digest: String,
    /// Every layer's probe, in layer order; all passed.
    pub probes: Vec<ImageLayerProbe>,
}

/// A failed build with every layer's probe outcome, in layer order (`NotRun`
/// for layers whose probe did not run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedImageBuild {
    pub failure: ImageBuildFailure,
    pub probes: Vec<ImageLayerProbe>,
    /// The layer whose build step failed, when the builder identifies it.
    pub failed_step: Option<String>,
}

impl FailedImageBuild {
    pub fn class(&self) -> ImageBuildFailureClass {
        self.failure.class
    }
}

/// Host-local settings of an owned BuildKit builder (`Host.spec.image_build_capacity`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedBuilderSettings {
    /// Name of the builder instance the daemon owns on this host.
    pub builder: String,
    /// The builder tool's state directory holding that instance. Each build
    /// uses a private tool configuration, which reaches the instance through it.
    pub state_directory: PathBuf,
    /// Private parent directory for per-build Dockerfiles and contexts; each
    /// build's directory is removed when the build ends or is cancelled.
    pub scratch: PathBuf,
}
