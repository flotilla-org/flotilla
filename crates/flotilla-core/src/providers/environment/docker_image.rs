//! Docker cache and Buildx implementations. Tool configuration is operation-local.
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_resources::{
    is_image_digest, HostImageAction, ImageBuildFailure, ImageBuildFailureClass, ImageLayerParent, PlacedImageIdentity,
};
use sha2::{Digest, Sha256};

use super::{
    docker::DockerEnvironmentProvider,
    image::{BuiltImage, ImageComposition, ImageLayerBuild},
    ImageBuilder, LocalImageCache, RegistryAuth,
};
use crate::providers::{ChannelLabel, CommandRunner};

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
    cache_name: Option<String>,
}
impl BuildxImageBuilder {
    async fn output_with_timeout(
        &self,
        args: &[&str],
        context: &Path,
        timeout: Duration,
    ) -> Result<crate::providers::CommandOutput, String> {
        tokio::time::timeout(timeout, self.runner.run_output("docker", args, context, &ChannelLabel::Default))
            .await
            .map_err(|_| format!("Docker process exceeded wall-clock deadline of {} seconds", timeout.as_secs()))?
    }

    pub fn for_cache(runner: Arc<dyn CommandRunner>, name: String) -> Self {
        Self { runner, cache_name: Some(name) }
    }
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner, cache_name: None }
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
    async fn build_layer(
        &self,
        request: ImageLayerBuild<'_>,
        auth: Option<&RegistryAuth>,
        log: &mut String,
    ) -> Result<(PlacedImageIdentity, BTreeSet<String>), ImageBuildFailure> {
        let ImageLayerBuild { name, spec, parent_digest, context, cache_name, probe_script } = request;
        let deterministic = |reason| ImageBuildFailure { class: ImageBuildFailureClass::Deterministic, reason };
        let transient = |reason| ImageBuildFailure { class: ImageBuildFailureClass::Transient, reason: RegistryAuth::redact(auth, reason) };
        if self.cache_name.as_deref().is_some_and(|name| name != cache_name) || cache_name.is_empty() {
            return Err(deterministic("build requires its bound named cache".into()));
        }
        if let Some(auth) = auth {
            let base = match &spec.layer.spec.parent {
                ImageLayerParent::Image(image) => image.as_str(),
                _ => parent_digest,
            };
            auth.validate(base, HostImageAction::ImagePull).map_err(deterministic)?;
        }
        let config = RegistryAuth::config(auth).map_err(transient)?;
        let config_arg = config.path().to_str().ok_or_else(|| deterministic("Docker config path is not UTF-8".into()))?;
        let architecture = match spec.inputs.architecture.as_str() {
            "x86_64" | "amd64" => "amd64",
            "aarch64" | "arm64" => "arm64",
            other => other,
        };
        let tag = format!("flotilla-build:{:x}", Sha256::digest(format!("{name}\0{}", spec.recipe_key)));
        let mut args = vec![
            "--config".into(),
            config_arg.into(),
            "buildx".into(),
            "build".into(),
            "--builder".into(),
            "default".into(),
            "--load".into(),
            "--progress=plain".into(),
            "--platform".into(),
            format!("linux/{architecture}"),
            "--tag".into(),
            tag.clone(),
            "--file".into(),
            spec.layer.spec.fragment.clone(),
        ];
        let base = match &spec.layer.spec.parent {
            ImageLayerParent::Image(image) => image.clone(),
            _ => {
                // Buildx's default Docker driver consumes images from the host
                // store. Give the exact local ID a content-derived label for FROM;
                // record the ID, never this label, as the parent identity.
                let label = format!("flotilla-parent:{:x}", Sha256::digest(parent_digest));
                self.runner
                    .run_with_timeout(
                        "docker",
                        &["--config", config_arg, "image", "tag", parent_digest, &label],
                        context,
                        &ChannelLabel::Default,
                        CLI_TIMEOUT,
                    )
                    .await
                    .map_err(transient)?;
                label
            }
        };
        let mut build_args = spec.layer.spec.args.clone();
        build_args.extend(spec.layer.spec.pins.iter().map(|(name, pin)| (name.clone(), pin.value.clone())));
        build_args.insert("BASE".into(), base);
        for (name, value) in build_args {
            args.extend(["--build-arg".into(), format!("{name}={value}")]);
        }
        args.push(".".into());
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let cached = self
            .output_with_timeout(&["--config", config_arg, "image", "inspect", "--format", "{{.Id}}", &tag], context, CLI_TIMEOUT)
            .await
            .map_err(transient)?;
        if !cached.success() {
            let output = self.output_with_timeout(&args, context, BUILD_TIMEOUT).await.map_err(transient)?;
            log.push_str(&RegistryAuth::redact(auth, output.stdout.clone()));
            log.push_str(&RegistryAuth::redact(auth, output.stderr.clone()));
            if !output.success() {
                let reason = failure_summary(&RegistryAuth::redact(auth, output.stderr.clone()), output.exit_code);
                let class = classify_build_failure(output.exit_code, &reason);
                return Err(ImageBuildFailure { class, reason });
            }
        }
        let identity = self
            .runner
            .run_with_timeout(
                "docker",
                &["--config", config_arg, "image", "inspect", "--format", "{{.Id}}", &tag],
                context,
                &ChannelLabel::Default,
                CLI_TIMEOUT,
            )
            .await
            .map_err(transient)?;
        let identity = PlacedImageIdentity { local_image_id: identity.trim().into(), registry_digest: None };
        identity.validate().map_err(deterministic)?;
        let mut verified = BTreeSet::new();
        for provide in &spec.layer.spec.provides {
            let command = spec
                .layer
                .spec
                .probes
                .get(provide)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| deterministic(format!("no verification probe declared for {provide}")))?;
            let probe_name = format!("flotilla-probe-{:x}", Sha256::digest(format!("{name}\0{provide}")));
            let cpus = spec.reservation.cpu.to_string();
            // Positional arguments preserve arbitrary probe argv without shell
            // interpolation. Preludes and the probe share their export scope.
            let script = probe_script;
            let mut args = vec![
                "--config",
                config_arg,
                "run",
                "--rm",
                "--name",
                &probe_name,
                "--cpus",
                &cpus,
                "--memory",
                "512m",
                "--pids-limit",
                "128",
                "--pull=never",
                "--network=none",
                "--entrypoint",
                "sh",
                &identity.local_image_id,
                "-c",
                &script,
                "flotilla-provide-probe",
            ];
            args.extend(command.iter().map(String::as_str));
            let output = match self.output_with_timeout(&args, context, PROBE_TIMEOUT).await {
                Ok(output) => output,
                Err(reason) => {
                    // Dropping the Docker CLI alone does not stop its container.
                    let _ = self
                        .runner
                        .run_with_timeout(
                            "docker",
                            &["--config", config_arg, "rm", "--force", &probe_name],
                            context,
                            &ChannelLabel::Default,
                            CLI_TIMEOUT,
                        )
                        .await;
                    return Err(transient(reason));
                }
            };
            log.push_str(&RegistryAuth::redact(auth, output.stdout.clone()));
            log.push_str(&RegistryAuth::redact(auth, output.stderr.clone()));
            if !output.success() {
                let reason = failure_summary(&RegistryAuth::redact(auth, output.stderr.clone()), output.exit_code);
                // 125 is Docker's own failure, 126/127 are an unusable probe
                // command, and other nonzero codes belong to the probe itself.
                let class = if output.exit_code == Some(125) || output.exit_code.is_none() || output.exit_code == Some(137) {
                    classify_build_failure(output.exit_code, &reason)
                } else {
                    ImageBuildFailureClass::Deterministic
                };
                return Err(ImageBuildFailure {
                    class,
                    reason: format!("provide verification failed: {provide}: {reason}").chars().take(MAX_REASON_CHARS).collect(),
                });
            }
            verified.insert(provide.clone());
        }
        Ok((identity, verified))
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
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
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
        return ImageBuildFailureClass::Deterministic;
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
        ImageBuildFailureClass::Transient
    } else {
        ImageBuildFailureClass::Deterministic
    }
}
