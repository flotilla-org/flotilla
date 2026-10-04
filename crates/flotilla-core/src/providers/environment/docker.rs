//! Docker-backed environment provider.
//!
//! Shells out to the `docker` CLI via `CommandRunner`, consistent with how every
//! other provider interacts with external tools.

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
use sha2::{Digest, Sha256};

use super::{
    runner::DockerEnvironmentRunner, CreateOpts, EnvironmentHandle, EnvironmentProvider, EnvironmentToolAssetAccess,
    EnvironmentToolAssetKind, EnvironmentVariableUpdate, ImagePullPolicy, PreparedEnvironmentAuth, ProvisionedEnvironment,
    ProvisionedMount, ProvisionedMountMode,
};
use crate::providers::{ChannelLabel, CommandRunner};

/// Bump this when the short-term Dockerfile image fingerprint inputs change.
const DOCKERFILE_IMAGE_TAG_VERSION: &str = "v1";

/// The `PATH` Docker gives a container whose image does not set one.
const DOCKER_DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// ---------------------------------------------------------------------------
// DockerEnvironmentProvider
// ---------------------------------------------------------------------------

/// An `EnvironmentProvider` that manages Docker containers as sandbox environments.
pub struct DockerEnvironmentProvider {
    inner: Arc<DockerEnvironmentProviderInner>,
}

impl DockerEnvironmentProvider {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { inner: Arc::new(DockerEnvironmentProviderInner::new(runner)) }
    }
}

#[cfg(unix)]
fn host_user() -> String {
    // SAFETY: getuid and getgid are side-effect-free process identity queries.
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

#[async_trait]
impl EnvironmentProvider for DockerEnvironmentProvider {
    // TODO: This fingerprints the Dockerfile contents plus the spec path only.
    // It intentionally ignores the broader build context for now, so a version
    // bump may be needed if that approximation proves too weak in practice.
    async fn ensure_image(&self, spec: &EnvironmentSpec, repo_root: &Path) -> Result<ImageId, String> {
        match &spec.image {
            ImageSource::Dockerfile(path) => {
                let abs_path = if path.is_relative() { repo_root.join(path) } else { path.clone() };
                let tag = dockerfile_image_tag(path, &abs_path)?;
                if self.inner.image_exists(&tag, repo_root).await? {
                    return Ok(ImageId::new(tag));
                }
                let context_dir = abs_path.parent().unwrap_or(repo_root).to_string_lossy().into_owned();
                let path_str = abs_path.to_string_lossy().into_owned();
                self.inner
                    .runner
                    .run("docker", &["build", "-t", &tag, "-f", &path_str, &context_dir], repo_root, &ChannelLabel::Default)
                    .await?;
                Ok(ImageId::new(tag))
            }
            ImageSource::Registry(image) => {
                self.inner.runner.run("docker", &["pull", image], repo_root, &ChannelLabel::Default).await?;
                Ok(ImageId::new(image.clone()))
            }
        }
    }

    async fn create(&self, id: EnvironmentId, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String> {
        let container_name = format!("flotilla-env-{}", id);

        let requested_mounts = opts.provisioned_mounts;
        let mut provisioned_mounts = Vec::new();
        let mut tokens = opts.tokens;
        let docker_config = match &opts.prepared_auth {
            PreparedEnvironmentAuth::NoRegistryCredential => None,
            PreparedEnvironmentAuth::RegistryConfig { directory } => Some(directory.to_string()),
        };
        let mut pull_policy = opts.image_pull_policy.docker_value();
        // Docker replaces an image's variable outright when `-e` names it, so a
        // prepend with no caller-supplied value must start from the image's own.
        let mut image_environment: Option<HashMap<String, String>> = None;
        for tool in &opts.tools {
            for asset in &tool.assets {
                let mode = match asset.access {
                    EnvironmentToolAssetAccess::ReadOnly => ProvisionedMountMode::Ro,
                    EnvironmentToolAssetAccess::SharedWritable => ProvisionedMountMode::Rw,
                };
                let (host_path, environment_path) = match asset.kind {
                    EnvironmentToolAssetKind::UnixSocket => {
                        let host_parent = asset
                            .host_path
                            .as_path()
                            .parent()
                            .ok_or_else(|| format!("Unix socket asset {} has no host parent directory", asset.host_path))?;
                        let environment_parent =
                            asset.environment_path.as_path().parent().ok_or_else(|| {
                                format!("Unix socket asset {} has no environment parent directory", asset.environment_path)
                            })?;
                        (host_parent.to_path_buf(), environment_parent.to_path_buf())
                    }
                    EnvironmentToolAssetKind::File | EnvironmentToolAssetKind::Directory => {
                        (asset.host_path.as_path().to_path_buf(), asset.environment_path.as_path().to_path_buf())
                    }
                };
                if requested_mounts.iter().chain(&provisioned_mounts).any(|mount| mount.environment_path.as_path() == environment_path) {
                    return Err(format!("mount target {} is reserved for {}", environment_path.display(), asset.purpose));
                }
                provisioned_mounts.push(ProvisionedMount::new(host_path, environment_path, mode));
            }
            for update in &tool.environment {
                match update {
                    EnvironmentVariableUpdate::Set { name, value, purpose } => {
                        if tokens.iter().any(|(existing, _)| existing == name) {
                            return Err(format!("environment variable {name} is reserved for {purpose}"));
                        }
                        tokens.push((name.clone(), value.clone()));
                    }
                    EnvironmentVariableUpdate::PrependPath { name, value } => {
                        match tokens.iter_mut().find(|(existing, _)| existing == name) {
                            Some((_, existing)) => *existing = format!("{value}:{existing}"),
                            None => {
                                if image_environment.is_none() {
                                    image_environment = Some(
                                        self.inner
                                            .image_environment(image.as_str(), opts.image_pull_policy, docker_config.as_deref())
                                            .await?,
                                    );
                                    // The inspected image is now local; run exactly that one.
                                    pull_policy = ImagePullPolicy::Never.docker_value();
                                }
                                let base = image_environment
                                    .as_ref()
                                    .and_then(|environment| environment.get(name))
                                    .map(String::as_str)
                                    .or_else(|| (name == "PATH").then_some(DOCKER_DEFAULT_PATH))
                                    // An image that sets the variable to empty gets the bare
                                    // value, not a trailing `:` (which would mean the cwd).
                                    .filter(|base| !base.is_empty());
                                let combined = match base {
                                    Some(base) => format!("{value}:{base}"),
                                    None => value.clone(),
                                };
                                tokens.push((name.clone(), combined));
                            }
                        }
                    }
                }
            }
        }
        provisioned_mounts.extend(requested_mounts);
        let env_id_str = id.to_string();
        let image_str = image.as_str().to_string();
        let label_val = format!("flotilla.environment={}", id);
        let mounts_label_val =
            format!("flotilla.provisioned_mounts={}", serde_json::to_string(&provisioned_mounts).map_err(|err| err.to_string())?);
        let env_id_env = format!("FLOTILLA_ENVIRONMENT_ID={}", env_id_str);
        #[cfg(unix)]
        let user = host_user();

        let mut args = Vec::new();
        if let Some(config) = &docker_config {
            args.extend(["--config", config.as_str()]);
        }
        args.extend([
            "run",
            "-d",
            "--init",
            "--pull",
            pull_policy,
            "--name",
            &container_name,
            "--label",
            &label_val,
            "--label",
            &mounts_label_val,
            "-e",
            &env_id_env,
        ]);
        let cpu_limit = opts.cpu_limit.map(|limit| limit.to_string());
        if let Some(limit) = cpu_limit.as_deref() {
            args.extend(["--cpus", limit]);
        }
        #[cfg(unix)]
        args.extend(["--user", user.as_str()]);

        let mount_specs: Vec<(String, String)> = provisioned_mounts
            .iter()
            .map(|mount| {
                if protected_git_mount(mount) {
                    // --mount rejects a missing bind source. -v silently creates
                    // a directory, which would leave Git metadata unprotected.
                    if mount.host_path.to_string().contains(',') {
                        return Err(format!("protected Git mount path contains a comma: {}", mount.host_path));
                    }
                    return Ok((
                        "--mount".to_string(),
                        format!("type=bind,source={},target={},readonly", mount.host_path, mount.environment_path),
                    ));
                }
                let mode = match mount.mode {
                    ProvisionedMountMode::Ro => "ro",
                    ProvisionedMountMode::Rw => "rw",
                };
                Ok(("-v".to_string(), format!("{}:{}:{mode}", mount.host_path, mount.environment_path)))
            })
            .collect::<Result<_, String>>()?;
        for (flag, mount_spec) in &mount_specs {
            args.push(flag);
            args.push(mount_spec);
        }

        // Token env vars
        let token_env_strs: Vec<String> = tokens.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        for token_env in &token_env_strs {
            args.push("-e");
            args.push(token_env);
        }

        args.push(&image_str);
        args.push("sleep");
        args.push("infinity");

        self.inner.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Default).await?;
        let image_digest = match self.inner.image_digest(&container_name).await {
            Ok(digest) => digest,
            Err(error) => {
                let cleanup = self.inner.destroy(&container_name).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => format!("{error}; additionally failed to remove container {container_name}: {cleanup_error}"),
                });
            }
        };

        Ok(self.inner.provisioned_environment(id, image.clone(), Some(image_digest), container_name, provisioned_mounts))
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        let format = r#"{{.Names}}\t{{.Label "flotilla.environment"}}\t{{.Image}}\t{{.Label "flotilla.provisioned_mounts"}}"#;
        let output = self
            .inner
            .runner
            .run(
                "docker",
                &["ps", "-a", "--filter", "label=flotilla.environment", "--format", format],
                Path::new("/"),
                &ChannelLabel::Default,
            )
            .await?;

        let mut handles = Vec::new();
        for line in output.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() < 4 {
                tracing::warn!(raw = %line, "docker list output missing provisioned mount metadata");
                return Err("docker list output missing provisioned mount metadata".to_string());
            }
            let container_name = parts[0].to_string();
            let env_id = parts[1].to_string();
            let image = parts[2].to_string();
            if env_id.is_empty() {
                tracing::warn!(container = %container_name, raw = %line, "docker list output missing environment id");
                return Err("docker list output missing environment id".to_string());
            }
            let mount_metadata = parts[3].trim();
            if mount_metadata.is_empty() {
                tracing::warn!(container = %container_name, "docker list output missing provisioned mount metadata");
                return Err(format!("docker list output missing provisioned mount metadata for container {container_name}"));
            }
            let provisioned_mounts = match serde_json::from_str(mount_metadata) {
                Ok(mounts) => mounts,
                Err(err) => {
                    tracing::warn!(container = %container_name, err = %err, raw = %mount_metadata, "failed to parse provisioned mount metadata");
                    return Err(format!("failed to parse provisioned mount metadata for container {container_name}: {err}"));
                }
            };
            handles.push(self.inner.provisioned_environment(
                EnvironmentId::new(env_id),
                ImageId::new(image),
                None,
                container_name,
                provisioned_mounts,
            ));
        }

        Ok(handles)
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        self.inner.destroy(container_id).await
    }
}

fn protected_git_mount(mount: &ProvisionedMount) -> bool {
    mount.mode == ProvisionedMountMode::Ro
        && mount.host_path.as_path() == mount.environment_path.as_path()
        && mount.host_path.as_path().parent().is_some_and(|parent| parent.file_name().is_some_and(|name| name == ".git"))
        && mount.host_path.as_path().file_name().is_some_and(|name| name == "config" || name == "hooks")
}

fn dockerfile_image_tag(spec_path: &Path, abs_path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(abs_path).map_err(|err| format!("failed to read Dockerfile {}: {err}", abs_path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(DOCKERFILE_IMAGE_TAG_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(spec_path.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Ok(format!("flotilla-env-{:x}", digest))
}

struct DockerEnvironmentProviderInner {
    runner: Arc<dyn CommandRunner>,
}

impl DockerEnvironmentProviderInner {
    fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn image_exists(&self, tag: &str, cwd: &Path) -> Result<bool, String> {
        match self.runner.run("docker", &["image", "inspect", tag], cwd, &ChannelLabel::Default).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn provisioned_environment(
        self: &Arc<Self>,
        id: EnvironmentId,
        image: ImageId,
        image_digest: Option<String>,
        container_name: String,
        provisioned_mounts: Vec<ProvisionedMount>,
    ) -> EnvironmentHandle {
        let runner = Arc::new(DockerEnvironmentRunner::new(container_name.clone(), Arc::clone(&self.runner))) as Arc<dyn CommandRunner>;
        Arc::new(DockerProvisionedEnvironment {
            id,
            container_name,
            image,
            image_digest,
            inner: Arc::clone(self),
            runner,
            provisioned_mounts,
        })
    }

    /// Resolves `image` locally under `pull_policy` and returns the environment
    /// its config declares.
    async fn image_environment(
        &self,
        image: &str,
        pull_policy: ImagePullPolicy,
        docker_config: Option<&str>,
    ) -> Result<HashMap<String, String>, String> {
        let output = match pull_policy {
            ImagePullPolicy::Always => {
                self.pull(image, docker_config)
                    .await
                    .map_err(|error| format!("pulling image {image} to read its environment failed: {error}"))?;
                self.inspect_environment(image).await?
            }
            ImagePullPolicy::IfNotPresent => match self.inspect_environment(image).await {
                Ok(output) => output,
                Err(inspect_error) => {
                    self.pull(image, docker_config).await.map_err(|error| {
                        format!(
                            "image {image} could not be inspected ({inspect_error}) and pulling it to read its environment failed: {error}"
                        )
                    })?;
                    self.inspect_environment(image).await?
                }
            },
            ImagePullPolicy::Never => self
                .inspect_environment(image)
                .await
                .map_err(|error| format!("image {image} is not available locally and the pull policy is never: {error}"))?,
        };
        let entries: Option<Vec<String>> = serde_json::from_str(output.trim())
            .map_err(|error| format!("docker returned invalid environment for image {image}: {error}"))?;
        Ok(entries
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| entry.split_once('=').map(|(name, value)| (name.to_string(), value.to_string())))
            .collect())
    }

    async fn inspect_environment(&self, image: &str) -> Result<String, String> {
        self.runner
            .run("docker", &["image", "inspect", "--format", "{{json .Config.Env}}", image], Path::new("/"), &ChannelLabel::Default)
            .await
    }

    async fn pull(&self, image: &str, docker_config: Option<&str>) -> Result<(), String> {
        let mut args = Vec::new();
        if let Some(config) = docker_config {
            args.extend(["--config", config]);
        }
        args.extend(["pull", image]);
        self.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
    }

    async fn image_digest(&self, container_name: &str) -> Result<String, String> {
        let output = self
            .runner
            .run("docker", &["inspect", "--format", "{{.Image}}", container_name], Path::new("/"), &ChannelLabel::Default)
            .await?;
        let digest = output.trim();
        if !digest.starts_with("sha256:") || digest.len() == "sha256:".len() {
            return Err(format!("docker returned invalid image digest for container {container_name}: {digest:?}"));
        }
        Ok(digest.to_string())
    }

    async fn status(&self, container_name: &str) -> Result<EnvironmentStatus, String> {
        let raw = self
            .runner
            .run("docker", &["inspect", "--format", "{{.State.Status}}", container_name], Path::new("/"), &ChannelLabel::Default)
            .await?;
        let status = raw.trim();
        Ok(match status {
            "running" => EnvironmentStatus::Running,
            "created" | "restarting" => EnvironmentStatus::Starting,
            "paused" | "exited" | "dead" => EnvironmentStatus::Stopped,
            other => EnvironmentStatus::Failed(other.to_string()),
        })
    }

    async fn env_vars(&self, container_name: &str) -> Result<HashMap<String, String>, String> {
        let output =
            self.runner.run("docker", &["exec", container_name, "sh", "-lc", "env"], Path::new("/"), &ChannelLabel::Default).await?;

        // Note: `sh -lc env` output is line-delimited. Values containing newlines
        // (e.g. PEM certificates) will be silently truncated. Acceptable for now;
        // a structured query (docker inspect) could provide the full picture if needed.
        Ok(output
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect())
    }

    async fn destroy(&self, container_name: &str) -> Result<(), String> {
        self.runner.run("docker", &["rm", "-f", container_name], Path::new("/"), &ChannelLabel::Default).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DockerProvisionedEnvironment
// ---------------------------------------------------------------------------

/// A live handle to a Docker container environment.
pub struct DockerProvisionedEnvironment {
    id: EnvironmentId,
    container_name: String,
    image: ImageId,
    image_digest: Option<String>,
    inner: Arc<DockerEnvironmentProviderInner>,
    runner: Arc<dyn CommandRunner>,
    provisioned_mounts: Vec<ProvisionedMount>,
}

#[async_trait]
impl ProvisionedEnvironment for DockerProvisionedEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
    }

    fn image_digest(&self) -> Option<&str> {
        self.image_digest.as_deref()
    }

    fn container_name(&self) -> Option<&str> {
        Some(&self.container_name)
    }

    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        self.provisioned_mounts.clone()
    }

    async fn status(&self) -> Result<EnvironmentStatus, String> {
        self.inner.status(&self.container_name).await
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        self.inner.env_vars(&self.container_name).await
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }

    async fn destroy(&self) -> Result<(), String> {
        self.inner.destroy(&self.container_name).await
    }
}
