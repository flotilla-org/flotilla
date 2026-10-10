//! Image capabilities are independent of environment lifecycle and executors.
use std::{collections::BTreeSet, path::Path};

use async_trait::async_trait;
use flotilla_resources::PlacedImageIdentity;

use super::RegistryAuth;

#[async_trait]
pub trait LocalImageCache: Send + Sync {
    async fn inspect(&self, digest: &str) -> Result<Option<PlacedImageIdentity>, String>;
    async fn has(&self, digest: &str) -> Result<bool, String> {
        Ok(self.inspect(digest).await?.is_some())
    }
    /// Read-only observation of a reference, including mutable baseline tags.
    /// This does not admit a tag for provisioning or relax digest-only inspect.
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
    /// Import the builder's portable Docker image archive into this cache.
    async fn load(&self, archive: &Path) -> Result<(), String>;
}

/// Composition inputs have already been frozen and acquired by the caller.
pub struct ImageComposition<'a> {
    pub context: &'a Path,
    pub fragment: &'a str,
    pub architecture: &'a str,
    pub args: &'a std::collections::BTreeMap<String, String>,
}

/// Frozen inputs and verification program supplied by orchestration; the
/// provider neither acquires source nor depends on agent/controller policy.
#[derive(bon::Builder)]
pub struct ImageLayerBuild<'a> {
    pub name: &'a str,
    pub spec: &'a flotilla_resources::ImageBuildSpec,
    pub parent_digest: &'a str,
    pub context: &'a Path,
    pub cache_name: &'a str,
    pub probe_script: &'a str,
}

/// Executor-neutral builder output; the archive is owned by the caller.
#[derive(Debug)]
pub struct BuiltImage {
    pub digest: String,
    pub archive: std::path::PathBuf,
}

#[async_trait]
pub trait ImageBuilder: Send + Sync {
    /// Build and verify a frozen layer in this builder's explicitly bound cache.
    /// The default Docker Buildx driver loads directly into that bound cache.
    async fn build_layer(
        &self,
        request: ImageLayerBuild<'_>,
        auth: Option<&RegistryAuth>,
        log: &mut String,
    ) -> Result<(PlacedImageIdentity, BTreeSet<String>), flotilla_resources::ImageBuildFailure>;
    async fn build(&self, composition: ImageComposition<'_>, archive: &Path, auth: Option<&RegistryAuth>) -> Result<BuiltImage, String>;
    async fn load(&self, image: &BuiltImage, cache_name: &str, cache: &dyn LocalImageCache) -> Result<(), String>;
    /// Publish an already held immutable image, without changing build evidence.
    async fn publish(&self, digest: &str, repository: &str, auth: Option<&RegistryAuth>) -> Result<String, String>;
    async fn push(&self, image: &BuiltImage, repository: &str, auth: Option<&RegistryAuth>) -> Result<String, String>;
}
