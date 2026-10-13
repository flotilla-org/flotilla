//! Docker cache and Buildx implementations. Tool configuration is operation-local.
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_resources::{is_image_digest, HostImageAction, ImageBuildFailureClass, PlacedImageIdentity};

use super::{
    docker::DockerEnvironmentProvider,
    image::{BuiltImage, ImageComposition},
    ImageBuilder, LocalImageCache, RegistryAuth,
};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;

pub fn validate_digest(reference: &str) -> Result<(), String> {
    if is_image_digest(reference) || reference.rsplit_once('@').is_some_and(|(repo, digest)| !repo.is_empty() && is_image_digest(digest)) {
        Ok(())
    } else {
        Err("image cache operation requires a digest".into())
    }
}

async fn run(runner: &dyn CommandRunner, args: &[&str], auth: Option<&RegistryAuth>) -> Result<String, String> {
    // Only registry operations need isolated credentials. Local cache commands
    // use the owning provider's endpoint without allocating a throwaway path.
    let config = if auth.is_some() || matches!(args.first(), Some(&"pull" | &"push")) { Some(RegistryAuth::config(auth)?) } else { None };
    let directory = config.as_ref().map(|config| config.path().to_string_lossy());
    let mut private = Vec::new();
    if let Some(directory) = &directory {
        private.extend(["--config", directory.as_ref()]);
    }
    private.extend_from_slice(args);
    runner
        .run_with_timeout("docker", &private, Path::new("/"), &ChannelLabel::Default, Duration::from_secs(30 * 60))
        .await
        .map(|text| RegistryAuth::redact(auth, text))
        .map_err(|text| RegistryAuth::redact(auth, text))
}

#[async_trait]
impl LocalImageCache for DockerEnvironmentProvider {
    async fn inspect(&self, reference: &str) -> Result<Option<PlacedImageIdentity>, String> {
        validate_digest(reference)?;
        // Do not collapse transport/runtime errors into an absent image.
        let present = if reference.contains('@') {
            // Shared content and archive loads do not attest a repository.
            // Query exact RepoDigests before inspect so these are absence, while
            // transport errors remain errors.
            let references = run(
                self.inner.runner.as_ref(),
                &["image", "ls", "--no-trunc", "--digests", "--format", "{{.Repository}}@{{.Digest}}"],
                None,
            )
            .await?;
            references.lines().any(|held| held.trim() == reference)
        } else {
            self.inventory().await?.contains(reference)
        };
        if !present {
            return Ok(None);
        }
        let output = run(self.inner.runner.as_ref(), &["image", "inspect", "--format", "{{json .}}", reference], None).await?;
        let value: serde_json::Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
        let local_image_id =
            value["Id"].as_str().filter(|id| is_image_digest(id)).ok_or("Docker inspect has no valid image ID")?.to_string();
        let registry_digest = value["RepoDigests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .find(|r| *r == reference)
            .and_then(|r| r.rsplit_once('@'))
            .map(|(_, d)| d.to_string());
        if is_image_digest(reference) && local_image_id != reference {
            return Err("cache inspection returned a different local digest".into());
        }
        if let Some((_, digest)) = reference.rsplit_once('@') {
            // The inventory admitted the exact reference. Refuse a changed or
            // malformed attestation instead of accepting partial evidence.
            if registry_digest.as_deref() != Some(digest) {
                return Err("cache inspection returned a different registry digest".into());
            }
        }
        Ok(Some(PlacedImageIdentity { local_image_id, registry_digest }))
    }
    async fn inspect_reference(&self, reference: &str) -> Result<Option<PlacedImageIdentity>, String> {
        let output =
            self.inner.runner.run_output("docker", &["image", "inspect", reference], Path::new("/"), &ChannelLabel::Default).await?;
        if !output.success() {
            return Ok(None);
        }
        let value: serde_json::Value = serde_json::from_str(&output.stdout).map_err(|error| error.to_string())?;
        let image = value.get(0).ok_or("Docker inspection returned no image")?;
        Ok(Some(PlacedImageIdentity {
            local_image_id: image["Id"].as_str().ok_or("Docker inspection returned no image ID")?.to_string(),
            registry_digest: image["RepoDigests"]
                .as_array()
                .and_then(|digests| digests.first())
                .and_then(|digest| digest.as_str())
                .map(str::to_owned),
        }))
    }
    async fn registry_available(&self, reference: &str) -> Result<(), String> {
        self.inner.runner.run("docker", &["manifest", "inspect", reference], Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
    }
    async fn pull(&self, reference: &str, auth: Option<&RegistryAuth>) -> Result<(), String> {
        validate_digest(reference)?;
        if !reference.contains('@') {
            return Err("pull requires reference@digest".into());
        }
        if let Some(auth) = auth {
            auth.validate(reference, HostImageAction::ImagePull)?;
        }
        run(self.inner.runner.as_ref(), &["pull", reference], auth).await.map(|_| ())
    }
    async fn inventory(&self) -> Result<BTreeSet<String>, String> {
        let output =
            run(self.inner.runner.as_ref(), &["image", "ls", "--no-trunc", "--digests", "--format", "{{.ID}} {{.Digest}}"], None).await?;
        Ok(output.split_whitespace().filter(|s| is_image_digest(s)).map(String::from).collect())
    }
    async fn remove(&self, digest: &str) -> Result<(), String> {
        validate_digest(digest)?;
        run(self.inner.runner.as_ref(), &["image", "rm", digest], None).await.map(|_| ())
    }
    async fn load(&self, archive: &Path) -> Result<(), String> {
        run(self.inner.runner.as_ref(), &["image", "load", "--input", archive.to_str().ok_or("archive path is not UTF-8")?], None)
            .await
            .map(|_| ())
    }
}

pub struct BuildxImageBuilder {
    runner: Arc<dyn CommandRunner>,
}
impl BuildxImageBuilder {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}
#[async_trait]
impl ImageBuilder for BuildxImageBuilder {
    async fn publish(&self, digest: &str, repository: &str, auth: Option<&RegistryAuth>) -> Result<String, String> {
        validate_digest(digest)?;
        if let Some(auth) = auth {
            auth.validate(repository, HostImageAction::ImagePush)?;
        }
        let tag = format!("{repository}:flotilla-{}", digest.trim_start_matches("sha256:"));
        run(self.runner.as_ref(), &["image", "tag", digest, &tag], None).await?;
        let output = run(self.runner.as_ref(), &["push", &tag], auth).await?;
        let digest = output
            .split_once("digest:")
            .and_then(|(_, t)| t.split_whitespace().next())
            .filter(|d| is_image_digest(d))
            .ok_or("registry push did not report a manifest digest")?;
        Ok(format!("{repository}@{digest}"))
    }
    async fn build(&self, c: ImageComposition<'_>, archive: &Path, auth: Option<&RegistryAuth>) -> Result<BuiltImage, String> {
        if let Some(auth) = auth {
            auth.validate(c.args.get("BASE").ok_or("authenticated composition requires a BASE reference")?, HostImageAction::ImagePull)?;
        }
        if archive.to_str().is_none_or(|p| p.contains(',')) {
            return Err("archive path is not representable in Buildx output attributes".into());
        }
        let config = RegistryAuth::config(auth)?;
        let metadata = archive.with_extension("metadata.json");
        let platform = format!("linux/{}", c.architecture);
        let output = format!("type=docker,dest={}", archive.display());
        let mut args = vec![
            "--config".into(),
            config.path().to_string_lossy().into_owned(),
            "buildx".into(),
            "build".into(),
            "--progress=plain".into(),
            "--platform".into(),
            platform,
            "--file".into(),
            c.fragment.into(),
            "--output".into(),
            output,
            "--metadata-file".into(),
            metadata.to_string_lossy().into_owned(),
        ];
        for (key, value) in c.args {
            args.extend(["--build-arg".into(), format!("{key}={value}")]);
        }
        args.push(".".into());
        self.runner
            .run_with_timeout(
                "docker",
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                c.context,
                &ChannelLabel::Default,
                Duration::from_secs(1800),
            )
            .await
            .map_err(|text| RegistryAuth::redact(auth, text))?;
        let result = tokio::fs::read(&metadata).await.map_err(|e| e.to_string());
        let _ = tokio::fs::remove_file(&metadata).await;
        let value: serde_json::Value = serde_json::from_slice(&result?).map_err(|e| e.to_string())?;
        let digest = value["containerimage.config.digest"]
            .as_str()
            .filter(|d| is_image_digest(d))
            .ok_or("Buildx has no valid config digest")?
            .into();
        Ok(BuiltImage { digest, archive: archive.into() })
    }
    async fn load(&self, image: &BuiltImage, cache_name: &str, cache: &dyn LocalImageCache) -> Result<(), String> {
        if cache_name.is_empty() {
            return Err("load requires a named cache".into());
        }
        validate_digest(&image.digest)?;
        cache.load(&image.archive).await?;
        if !cache.has(&image.digest).await? {
            return Err("loaded image does not match builder digest".into());
        }
        Ok(())
    }
    async fn push(&self, image: &BuiltImage, repository: &str, auth: Option<&RegistryAuth>) -> Result<String, String> {
        run(self.runner.as_ref(), &["image", "load", "--input", image.archive.to_str().ok_or("archive path is not UTF-8")?], None).await?;
        self.publish(&image.digest, repository, auth).await
    }
}

pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REASON_CHARS: usize = 2048;

// Status holds a bounded diagnostic, while the Artifact retains the full output.
// BuildKit's final ERROR summary follows the progress stream. Never classify
// arbitrary Dockerfile/probe output as infrastructure evidence.
pub fn failure_summary(stderr: &str, exit_code: Option<i32>) -> String {
    let tail = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let summary = if tail.is_empty() { format!("Docker process exited with {exit_code:?}") } else { tail.to_string() };
    summary.chars().take(MAX_REASON_CHARS).collect()
}

pub fn classify_build_failure(exit_code: Option<i32>, reason: &str) -> ImageBuildFailureClass {
    let reason = failure_summary(reason, exit_code).to_ascii_lowercase();
    if exit_code.is_some() && exit_code != Some(137) && reason.contains("did not complete successfully") && reason.contains("exit code:") {
        return ImageBuildFailureClass::Recipe;
    }
    let infrastructure = [
        "connection reset",
        "connection refused",
        "no space left on device",
        "temporary failure in name resolution",
        "no such host",
        "tls handshake timeout",
        "cannot connect to the docker daemon",
        "network is unreachable",
        "service unavailable",
        "code = unavailable",
        "unexpected eof",
        "i/o timeout",
        "context deadline exceeded",
        "http 429",
        "status code: 429",
        "429 too many requests",
    ];
    if exit_code.is_none() || exit_code == Some(137) || infrastructure.iter().any(|signal| reason.contains(signal)) {
        ImageBuildFailureClass::Infrastructure
    } else {
        ImageBuildFailureClass::Recipe
    }
}
