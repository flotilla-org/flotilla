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
    EnvironmentToolAssetKind, EnvironmentVariableUpdate, ProvisionedEnvironment, ProvisionedMount, ProvisionedMountMode,
};
use crate::providers::{ChannelLabel, CommandRunner};

/// Bump this when the short-term Dockerfile image fingerprint inputs change.
const DOCKERFILE_IMAGE_TAG_VERSION: &str = "v1";

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
                    .run("docker", &["build", "-t", &tag, "-f", &path_str, &context_dir], repo_root, &ChannelLabel::Noop)
                    .await?;
                Ok(ImageId::new(tag))
            }
            ImageSource::Registry(image) => {
                self.inner.runner.run("docker", &["pull", image], repo_root, &ChannelLabel::Noop).await?;
                Ok(ImageId::new(image.clone()))
            }
        }
    }

    async fn create(&self, id: EnvironmentId, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String> {
        let container_name = format!("flotilla-env-{}", id);

        let requested_mounts = opts.provisioned_mounts;
        let mut provisioned_mounts = Vec::new();
        let mut tokens = opts.tokens;
        for tool in &opts.tools {
            for asset in &tool.assets {
                if requested_mounts.iter().chain(&provisioned_mounts).any(|mount| mount.environment_path == asset.environment_path) {
                    return Err(format!("mount target {} is reserved for {}", asset.environment_path, asset.purpose));
                }
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
                            None => tokens.push((name.clone(), value.clone())),
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

        let docker_config = opts.docker_config_dir.as_ref().map(ToString::to_string);
        let mut args = Vec::new();
        if let Some(config) = &docker_config {
            args.extend(["--config", config.as_str()]);
        }
        args.extend([
            "run",
            "-d",
            "--pull",
            opts.image_pull_policy.docker_value(),
            "--name",
            &container_name,
            "--label",
            &label_val,
            "--label",
            &mounts_label_val,
            "-e",
            &env_id_env,
        ]);
        #[cfg(unix)]
        args.extend(["--user", user.as_str()]);

        let mount_specs: Vec<String> = provisioned_mounts
            .iter()
            .map(|mount| {
                let mode = match mount.mode {
                    ProvisionedMountMode::Ro => "ro",
                    ProvisionedMountMode::Rw => "rw",
                };
                format!("{}:{}:{mode}", mount.host_path, mount.environment_path)
            })
            .collect();
        for mount_spec in &mount_specs {
            args.push("-v");
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

        self.inner.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Noop).await?;
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
            .run("docker", &["ps", "-a", "--filter", "label=flotilla.environment", "--format", format], Path::new("/"), &ChannelLabel::Noop)
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
        match self.runner.run("docker", &["image", "inspect", tag], cwd, &ChannelLabel::Noop).await {
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

    async fn image_digest(&self, container_name: &str) -> Result<String, String> {
        let output =
            self.runner.run("docker", &["inspect", "--format", "{{.Image}}", container_name], Path::new("/"), &ChannelLabel::Noop).await?;
        let digest = output.trim();
        if !digest.starts_with("sha256:") || digest.len() == "sha256:".len() {
            return Err(format!("docker returned invalid image digest for container {container_name}: {digest:?}"));
        }
        Ok(digest.to_string())
    }

    async fn status(&self, container_name: &str) -> Result<EnvironmentStatus, String> {
        let raw = self
            .runner
            .run("docker", &["inspect", "--format", "{{.State.Status}}", container_name], Path::new("/"), &ChannelLabel::Noop)
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
        let output = self.runner.run("docker", &["exec", container_name, "sh", "-lc", "env"], Path::new("/"), &ChannelLabel::Noop).await?;

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
        self.runner.run("docker", &["rm", "-f", container_name], Path::new("/"), &ChannelLabel::Noop).await?;
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
