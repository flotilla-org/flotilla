//! Docker cache and Buildx implementations. Tool configuration is operation-local.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_resources::{
    image_identity, is_image_digest, HostImageAction, ImageBuildFailure, ImageBuildFailureClass, ImageLayerProbe, ImageProbeOutcome,
    PlacedImageIdentity,
};

use super::{
    docker::DockerEnvironmentProvider,
    image::{BuiltImage, FailedImageBuild, ImageBuildRequest, ImageBuildService, ImageDeliveryTarget, OwnedBuilderSettings},
    image_dockerfile::{generate_build, GeneratedBuild},
    LocalImageCache, RegistryAuth,
};

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
}

/// The owned-BuildKit implementation of [`ImageBuildService`]: one generated
/// multi-stage Dockerfile per identity × architecture, built by the host's
/// owned Buildx builder and either pushed with a registry build cache or loaded
/// into the local store, then probed layer by layer on the result.
pub struct OwnedBuildKitService {
    runner: Arc<dyn CommandRunner>,
    settings: OwnedBuilderSettings,
}

impl OwnedBuildKitService {
    pub fn new(runner: Arc<dyn CommandRunner>, settings: OwnedBuilderSettings) -> Self {
        Self { runner, settings }
    }
}

/// Unprivileged probe identity and its writable home.
pub const PROBE_USER: &str = "65534:65534";
pub const PROBE_HOME: &str = "/tmp/flotilla-probe-home";
/// Local tag repository for single-host (`--load`) builds.
pub const LOCAL_IMAGE_REPOSITORY: &str = "flotilla-image";

/// `<first 40 hex of identity>-<architecture>`, matching the ImageBuild name.
pub fn image_tag(identity: &str, architecture: &str) -> String {
    let hex = identity.strip_prefix("sha256:").unwrap_or(identity);
    format!("{}-{architecture}", &hex[..hex.len().min(40)])
}

fn failure(class: ImageBuildFailureClass, reason: impl Into<String>) -> ImageBuildFailure {
    ImageBuildFailure { class, reason: reason.into().chars().take(MAX_REASON_CHARS).collect() }
}
fn recipe(reason: impl Into<String>) -> ImageBuildFailure {
    failure(ImageBuildFailureClass::Recipe, reason)
}
fn infrastructure(reason: impl Into<String>) -> ImageBuildFailure {
    failure(ImageBuildFailureClass::Infrastructure, reason)
}

/// A private per-build directory. Dropping it removes the Dockerfile, the
/// contexts and the metadata, including when the build is cancelled.
struct BuildWorkspace(PathBuf);
impl Drop for BuildWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl BuildWorkspace {
    fn create(scratch: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(scratch).map_err(|error| format!("image build scratch {}: {error}", scratch.display()))?;
        let path = scratch.join(format!("flotilla-image-build-{}", uuid::Uuid::new_v4().simple()));
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&path)
        };
        #[cfg(not(unix))]
        let created = std::fs::create_dir(&path);
        created.map_err(|error| format!("image build directory {}: {error}", path.display()))?;
        Ok(Self(path))
    }

    fn dockerfile(&self) -> PathBuf {
        self.0.join("Dockerfile")
    }
    fn metadata(&self) -> PathBuf {
        self.0.join("metadata.json")
    }
    /// The (empty) main build context; fragments reach files only through
    /// their layer's named context.
    fn root(&self) -> PathBuf {
        self.0.join("root")
    }
    fn context(&self, name: &str) -> PathBuf {
        self.0.join("contexts").join(name)
    }

    fn materialise(&self, generated: &GeneratedBuild) -> Result<(), String> {
        let io = |error: std::io::Error| error.to_string();
        std::fs::write(self.dockerfile(), &generated.dockerfile).map_err(io)?;
        std::fs::create_dir_all(self.root()).map_err(io)?;
        for context in &generated.contexts {
            let directory = self.context(&context.name);
            std::fs::create_dir_all(&directory).map_err(io)?;
            for (relative, file) in &context.files {
                // Paths are relative and normalised (layer validation).
                let path = directory.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(io)?;
                }
                std::fs::write(&path, &file.content).map_err(io)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if file.executable { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).map_err(io)?;
                }
            }
        }
        Ok(())
    }
}

fn path_arg(path: &Path) -> Result<String, ImageBuildFailure> {
    path.to_str().map(str::to_owned).ok_or_else(|| infrastructure(format!("path {} is not UTF-8", path.display())))
}

impl OwnedBuildKitService {
    /// Run one Docker CLI command under `timeout`.
    ///
    /// Cancellation contract: an attempt ends early when its deadline passes
    /// or when the caller drops the build future (the ImageBuild was deleted,
    /// or the job was aborted). Either way the running command's future is
    /// dropped, and the runner's graceful seam sends SIGTERM to the Docker
    /// CLI's process group, escalating to SIGKILL after [`CANCEL_GRACE`].
    /// The Buildx plugin shares that group (and the CLI also forwards the
    /// signal by closing its plugin socket); Buildx cancels its BuildKit
    /// solve on SIGTERM, so the build inside the owned docker-container
    /// builder stops rather than running on unobserved. A SIGKILL would only
    /// drop the client connection. `buildx stop` is deliberately not used: it
    /// stops the whole builder, killing every other slot's build. A dropped
    /// probe's `docker run` forwards the signal to its container (sig-proxy)
    /// and the container is `--rm`; a probe deadline also force-removes it.
    async fn docker(&self, args: &[String], timeout: Duration) -> Result<Result<CommandOutput, String>, ()> {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let command = self.runner.run_output_with_grace("docker", &args, Path::new("/"), &ChannelLabel::Default, CANCEL_GRACE);
        tokio::time::timeout(timeout, command).await.map_err(|_| ())
    }

    async fn execute(
        &self,
        request: &ImageBuildRequest<'_>,
        probes: &mut [ImageLayerProbe],
        failed_step: &mut Option<String>,
        log: &mut String,
    ) -> Result<String, ImageBuildFailure> {
        let ImageBuildRequest { identity, architecture, layers, delivery, secrets, probe_wrapper, probe_cpus } = request;
        if !matches!(*architecture, "amd64" | "arm64") {
            return Err(recipe(format!("image build architecture `{architecture}` must be amd64 or arm64")));
        }
        if image_identity(layers) != *identity {
            return Err(recipe("image build identity does not match its layers"));
        }
        if *probe_cpus == 0 || probe_wrapper.trim().is_empty() {
            return Err(recipe("image build needs a probe wrapper and at least one probe CPU"));
        }
        let generated = generate_build(layers, architecture).map_err(recipe)?;
        let mut secret_args = Vec::new();
        for name in &generated.secrets {
            // Ungranted secrets are deterministic until the installation's
            // grants change, so they do not retry identical inputs.
            let file = secrets.get(name).ok_or_else(|| recipe(format!("build secret `{name}` is not granted to this build")))?;
            let source = path_arg(file.path())?;
            if source.contains(',') {
                return Err(infrastructure(format!("build secret `{name}` file path is not representable in Buildx")));
            }
            secret_args.extend(["--secret".to_string(), format!("id={name},src={source}")]);
        }
        let auth = match delivery {
            ImageDeliveryTarget::Push { repository, cache_repository, auth } => {
                if repository.trim().is_empty() || cache_repository.trim().is_empty() || repository.contains('@') {
                    return Err(recipe("registry delivery needs an image repository and a cache repository"));
                }
                auth.validate(repository, HostImageAction::ImagePush).map_err(recipe)?;
                Some(auth)
            }
            ImageDeliveryTarget::Load => None,
        };
        let redact = |text: String| RegistryAuth::redact(auth, text);
        // One private tool configuration for the whole operation: the push
        // credential (or none) and a link to the owned builder's state.
        let config = RegistryAuth::config(auth).map_err(|error| infrastructure(redact(error)))?;
        link_builder_state(config.path(), &self.settings.state_directory).map_err(infrastructure)?;
        let config_arg = path_arg(config.path())?;
        self.ensure_builder(&config_arg, log).await?;
        let workspace = BuildWorkspace::create(&self.settings.scratch).map_err(infrastructure)?;
        workspace.materialise(&generated).map_err(infrastructure)?;

        let tag = image_tag(identity, architecture);
        let mut args = vec![
            "--config".to_string(),
            config_arg.clone(),
            "buildx".into(),
            "build".into(),
            "--builder".into(),
            self.settings.builder.clone(),
            "--platform".into(),
            format!("linux/{architecture}"),
            "--file".into(),
            path_arg(&workspace.dockerfile())?,
        ];
        for context in &generated.contexts {
            args.extend(["--build-context".to_string(), format!("{}={}", context.name, path_arg(&workspace.context(&context.name))?)]);
        }
        args.extend(secret_args);
        args.extend([
            "--provenance=false".to_string(),
            "--metadata-file".into(),
            path_arg(&workspace.metadata())?,
            "--progress=plain".into(),
        ]);
        match delivery {
            ImageDeliveryTarget::Push { repository, cache_repository, .. } => args.extend([
                "--push".to_string(),
                "--tag".into(),
                format!("{repository}:{tag}"),
                "--cache-to".into(),
                format!("type=registry,ref={cache_repository},mode=max"),
                "--cache-from".into(),
                format!("type=registry,ref={cache_repository}"),
            ]),
            ImageDeliveryTarget::Load => args.extend(["--load".to_string(), "--tag".into(), format!("{LOCAL_IMAGE_REPOSITORY}:{tag}")]),
        }
        args.push(path_arg(&workspace.root())?);
        log.push_str(&format!("### build {identity} for linux/{architecture}\n"));
        let output = match self.docker(&args, BUILD_TIMEOUT).await {
            Err(()) => return Err(infrastructure(format!("image build exceeded its {} second deadline", BUILD_TIMEOUT.as_secs()))),
            Ok(Err(error)) => return Err(infrastructure(redact(error))),
            Ok(Ok(output)) => output,
        };
        log.push_str(&redact(output.stdout.clone()));
        log.push_str(&redact(output.stderr.clone()));
        if !output.success() {
            let stderr = redact(output.stderr);
            *failed_step = failing_step_layer(&stderr, layers);
            let reason = failure_summary(&stderr, output.exit_code);
            return Err(failure(classify_build_failure(output.exit_code, &stderr), reason));
        }

        let image = match delivery {
            ImageDeliveryTarget::Push { repository, .. } => {
                let metadata = std::fs::read(workspace.metadata()).map_err(|error| infrastructure(format!("Buildx metadata: {error}")))?;
                let value: serde_json::Value =
                    serde_json::from_slice(&metadata).map_err(|error| infrastructure(format!("Buildx metadata: {error}")))?;
                let digest = value["containerimage.digest"]
                    .as_str()
                    .filter(|digest| is_image_digest(digest))
                    .ok_or_else(|| infrastructure("Buildx metadata has no valid pushed image digest"))?;
                let reference = format!("{repository}@{digest}");
                // Probe exactly the pushed manifest, as providers will pull it.
                let pull = ["--config".to_string(), config_arg.clone(), "pull".into(), reference.clone()];
                match self.docker(&pull, BUILD_TIMEOUT).await {
                    Err(()) => return Err(infrastructure("pulling the pushed image exceeded its deadline")),
                    Ok(Err(error)) => return Err(infrastructure(redact(error))),
                    Ok(Ok(output)) if !output.success() => {
                        return Err(infrastructure(format!(
                            "pulling the pushed image failed: {}",
                            failure_summary(&redact(output.stderr), output.exit_code)
                        )))
                    }
                    Ok(Ok(_)) => {}
                }
                reference
            }
            ImageDeliveryTarget::Load => {
                let inspect = [
                    "--config".to_string(),
                    config_arg.clone(),
                    "image".into(),
                    "inspect".into(),
                    "--format".into(),
                    "{{.Id}}".into(),
                    format!("{LOCAL_IMAGE_REPOSITORY}:{tag}"),
                ];
                let output = match self.docker(&inspect, CLI_TIMEOUT).await {
                    Err(()) => return Err(infrastructure("inspecting the loaded image exceeded its deadline")),
                    Ok(Err(error)) => return Err(infrastructure(redact(error))),
                    Ok(Ok(output)) => output,
                };
                let id = output.stdout.trim();
                if !output.success() || !is_image_digest(id) {
                    return Err(infrastructure(format!(
                        "loaded image has no valid local ID: {}",
                        failure_summary(&redact(output.stderr), output.exit_code)
                    )));
                }
                // The tagged image is the delivered artifact of a local
                // (single-host) build: providers on this host run it by this
                // local image ID, so it is deliberately kept.
                id.to_string()
            }
        };
        drop(workspace);

        let probed = self.probe(request, &image, &config_arg, probes, log).await;
        if let ImageDeliveryTarget::Push { .. } = delivery {
            // The pull above existed only to probe the pushed manifest; the
            // registry holds the delivered image. Removing the local copy
            // does not change the build's outcome, so a failure is logged.
            let remove = ["--config".to_string(), config_arg.clone(), "image".into(), "rm".into(), image.clone()];
            let removed = match self.docker(&remove, CLI_TIMEOUT).await {
                Ok(Ok(output)) if output.success() => Ok(()),
                Ok(Ok(output)) => Err(failure_summary(&redact(output.stderr), output.exit_code)),
                Ok(Err(error)) => Err(redact(error)),
                Err(()) => Err("removal exceeded its deadline".into()),
            };
            if let Err(error) = removed {
                tracing::warn!(image = %image, %error, "could not remove the image pulled for probing");
                log.push_str(&format!("### could not remove the probed image {image}: {error}\n"));
            }
        }
        probed.map(|()| image)
    }

    /// Run every layer's probe, in order, on the delivered `image`.
    async fn probe(
        &self,
        request: &ImageBuildRequest<'_>,
        image: &str,
        config_arg: &str,
        probes: &mut [ImageLayerProbe],
        log: &mut String,
    ) -> Result<(), ImageBuildFailure> {
        let ImageBuildRequest { layers, delivery, probe_wrapper, probe_cpus, .. } = request;
        let auth = match delivery {
            ImageDeliveryTarget::Push { auth, .. } => Some(auth),
            ImageDeliveryTarget::Load => None,
        };
        let redact = |text: String| RegistryAuth::redact(auth, text);
        let cpus = probe_cpus.to_string();
        let mut failed = Vec::new();
        for (index, layer) in layers.iter().enumerate() {
            let name = format!("flotilla-probe-{}", uuid::Uuid::new_v4().simple());
            // Positional arguments carry the probe without shell interpolation;
            // the wrapper sources preludes and runs `"$@"` in the same shell.
            let home = format!("HOME={PROBE_HOME}");
            let home_mount = format!("{PROBE_HOME}:exec,uid=65534,gid=65534,mode=0700");
            let args = [
                "--config",
                config_arg,
                "run",
                "--rm",
                "--name",
                name.as_str(),
                "--network=none",
                "--user",
                PROBE_USER,
                "-e",
                home.as_str(),
                "--tmpfs",
                "/tmp:exec,mode=1777",
                "--tmpfs",
                home_mount.as_str(),
                "--pull=never",
                "--cpus",
                cpus.as_str(),
                "--memory",
                "512m",
                "--pids-limit",
                "256",
                "--entrypoint",
                "sh",
                image,
                "-c",
                probe_wrapper,
                "flotilla-layer-probe",
                "sh",
                "-c",
                layer.probe.as_str(),
            ]
            .map(str::to_owned);
            log.push_str(&format!("### probe {}\n", layer.name));
            let output = match self.docker(&args, PROBE_TIMEOUT).await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return Err(infrastructure(redact(error))),
                Err(()) => {
                    // Dropping the Docker client does not stop its container.
                    let remove = ["--config".to_string(), config_arg.to_string(), "rm".into(), "--force".into(), name];
                    let _ = self.docker(&remove, CLI_TIMEOUT).await;
                    let reason = format!("probe exceeded its {} second deadline", PROBE_TIMEOUT.as_secs());
                    probes[index].outcome = ImageProbeOutcome::Failed { reason: reason.clone() };
                    return Err(infrastructure(format!("layer `{}`: {reason}", layer.name)));
                }
            };
            log.push_str(&redact(output.stdout.clone()));
            log.push_str(&redact(output.stderr.clone()));
            if output.success() {
                probes[index].outcome = ImageProbeOutcome::Passed;
                continue;
            }
            let diagnostic = if output.stderr.trim().is_empty() { &output.stdout } else { &output.stderr };
            let reason = failure_summary(&redact(diagnostic.clone()), output.exit_code);
            // 125 is Docker's own failure and 137 a kill; other nonzero codes
            // (including 126/127, an unusable probe command) are the probe's.
            if matches!(output.exit_code, None | Some(125) | Some(137))
                && classify_build_failure(output.exit_code, &reason) == ImageBuildFailureClass::Infrastructure
            {
                return Err(infrastructure(format!("layer `{}` probe could not run: {reason}", layer.name)));
            }
            probes[index].outcome = ImageProbeOutcome::Failed { reason };
            failed.push(layer.name.as_str());
        }
        if !failed.is_empty() {
            return Err(recipe(format!("layer probes failed: {}", failed.join(", "))));
        }
        Ok(())
    }
}

impl OwnedBuildKitService {
    /// Bootstrap the owned builder instance when it is absent: `buildx inspect
    /// <name>` through the private configuration, and on failure `buildx create
    /// --name <name> --driver docker-container --bootstrap`. The instance lives
    /// in the injected state directory, so every later build reaches it through
    /// the same configuration link, and other Buildx users never see it.
    async fn ensure_builder(&self, config: &str, log: &mut String) -> Result<(), ImageBuildFailure> {
        let builder = self.settings.builder.as_str();
        let inspect = ["--config", config, "buildx", "inspect", builder].map(str::to_owned);
        match self.docker(&inspect, CLI_TIMEOUT).await {
            Ok(Ok(output)) if output.success() => return Ok(()),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(infrastructure(format!("inspecting the owned builder: {error}"))),
            Err(()) => return Err(infrastructure("inspecting the owned builder exceeded its deadline")),
        }
        log.push_str(&format!("### create owned builder {builder}\n"));
        let create =
            ["--config", config, "buildx", "create", "--name", builder, "--driver", "docker-container", "--bootstrap"].map(str::to_owned);
        match self.docker(&create, BUILDER_BOOTSTRAP_TIMEOUT).await {
            Ok(Ok(output)) if output.success() => Ok(()),
            Ok(Ok(output)) => {
                log.push_str(&output.stderr);
                Err(infrastructure(format!(
                    "creating the owned builder `{builder}` failed: {}",
                    failure_summary(&output.stderr, output.exit_code)
                )))
            }
            Ok(Err(error)) => Err(infrastructure(format!("creating the owned builder `{builder}`: {error}"))),
            Err(()) => Err(infrastructure(format!("creating the owned builder `{builder}` exceeded its deadline"))),
        }
    }
}

/// The layer whose build step failed, read from BuildKit's error summary
/// (` > [<stage> n/m] …`). Stage names are generated (`flotilla-layer-<i>`) or
/// declared aliases prefixed by their layer's name; anything else (the
/// frontend, context transfer, an export) belongs to no layer.
pub fn failing_step_layer(stderr: &str, layers: &[flotilla_resources::ImageLayerSpec]) -> Option<String> {
    let stage = stderr.lines().rev().find_map(|line| {
        let rest = line.trim_start().strip_prefix("> [")?;
        let (stage, _) = rest.split_once(' ')?;
        Some(stage.to_ascii_lowercase())
    })?;
    if let Some(index) = stage.strip_prefix("flotilla-layer-").and_then(|index| index.parse::<usize>().ok()) {
        return layers.get(index).map(|layer| layer.name.clone());
    }
    layers
        .iter()
        .filter(|layer| stage.strip_prefix(&layer.name.to_ascii_lowercase()).is_some_and(|rest| rest.starts_with('-')))
        .max_by_key(|layer| layer.name.len())
        .map(|layer| layer.name.clone())
}

/// Make the owned builder instance visible through the private configuration
/// (Buildx keeps instances under `<config>/buildx`). The state directory is
/// created private on first use.
fn link_builder_state(config: &Path, state: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state)
            .map_err(|error| format!("builder state {}: {error}", state.display()))?;
        std::os::unix::fs::symlink(state, config.join("buildx")).map_err(|error| format!("builder state link: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = (config, state);
        Err("owned BuildKit builds require a Unix host".into())
    }
}

#[async_trait]
impl ImageBuildService for OwnedBuildKitService {
    async fn build(&self, request: ImageBuildRequest<'_>, log: &mut String) -> Result<BuiltImage, FailedImageBuild> {
        let mut probes: Vec<_> =
            request.layers.iter().map(|layer| ImageLayerProbe { layer: layer.name.clone(), outcome: ImageProbeOutcome::NotRun }).collect();
        let mut failed_step = None;
        match self.execute(&request, &mut probes, &mut failed_step, log).await {
            Ok(digest) => Ok(BuiltImage { digest, probes }),
            Err(failure) => {
                log.push_str(&format!("\n{}\n", failure.reason));
                Err(FailedImageBuild { failure, probes, failed_step })
            }
        }
    }
}

pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a cancelled Docker command has to stop after SIGTERM before it is
/// killed; Buildx needs it to cancel its BuildKit solve.
pub const CANCEL_GRACE: Duration = Duration::from_secs(30);
/// Creating the builder pulls and starts the BuildKit container.
const BUILDER_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_REASON_CHARS: usize = 2048;

// Status holds a bounded diagnostic, while the Artifact retains the full output.
// BuildKit's final ERROR summary follows the progress stream. Never classify
// arbitrary Dockerfile/probe output as infrastructure evidence; only recognized
// apt/curl diagnostics before that summary can override a failed RUN step.
pub fn failure_summary(stderr: &str, exit_code: Option<i32>) -> String {
    let tail = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let summary = if tail.is_empty() { format!("Docker process exited with {exit_code:?}") } else { tail.to_string() };
    summary.chars().take(MAX_REASON_CHARS).collect()
}

// Recognize tool diagnostics, not RUN command text or arbitrary output that
// happens to mention an HTTP status. BuildKit prefixes output with a step and
// elapsed seconds; raw tool output is accepted too.
fn transient_fetch_failure(line: &str) -> bool {
    let line = line.trim();
    let line = line
        .split_once(' ')
        .and_then(|(step, rest)| {
            step.strip_prefix('#')?.parse::<u32>().ok()?;
            let (elapsed, diagnostic) = rest.trim_start().split_once(' ')?;
            elapsed.parse::<f64>().ok()?;
            Some(diagnostic.trim_start())
        })
        .unwrap_or(line);
    let apt = line.starts_with("e: failed to fetch ") || line.starts_with("w: failed to fetch ");
    let curl_http = line.starts_with("curl: (22)");
    if apt || curl_http {
        let status = line
            .split_whitespace()
            .find_map(|word| word.trim_matches(|c: char| !c.is_ascii_digit()).parse::<u16>().ok().filter(|code| (400..600).contains(code)));
        // Missing packages and authorization errors remain recipe failures.
        // An apt fetch failure without an HTTP status can be DNS/TLS/timeout.
        return status.map_or(apt, |code| code == 429 || code >= 500);
    }
    line.starts_with("curl: (7)") || line.starts_with("curl: (28)")
}

pub fn classify_build_failure(exit_code: Option<i32>, reason: &str) -> ImageBuildFailureClass {
    if reason.to_ascii_lowercase().lines().any(transient_fetch_failure) {
        return ImageBuildFailureClass::Infrastructure;
    }
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
