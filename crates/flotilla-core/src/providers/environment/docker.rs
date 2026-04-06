//! Docker-backed environment provider.
//!
//! Shells out to the `docker` CLI via `CommandRunner`, consistent with how every
//! other provider interacts with external tools.

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
use sha2::{Digest, Sha256};

use super::{runner::DockerEnvironmentRunner, CreateOpts, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment};
use crate::providers::{ChannelLabel, CommandRunner};

/// Fixed path inside the container where the daemon socket is mounted.
const CONTAINER_SOCKET_PATH: &str = "/run/flotilla.sock";
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

        let socket_str = opts.daemon_socket_path.to_string();
        let env_id_str = id.to_string();
        let image_str = image.as_str().to_string();
        let label_val = format!("flotilla.environment={}", id);
        let socket_mount = format!("{}:{CONTAINER_SOCKET_PATH}", socket_str);
        let env_id_env = format!("FLOTILLA_ENVIRONMENT_ID={}", env_id_str);
        let socket_env = format!("FLOTILLA_DAEMON_SOCKET={CONTAINER_SOCKET_PATH}");

        let mut args =
            vec!["run", "-d", "--name", &container_name, "--label", &label_val, "-v", &socket_mount, "-e", &socket_env, "-e", &env_id_env];

        // Optional reference_repo mount
        let reference_repo_mount;
        if let Some(ref repo) = opts.reference_repo {
            reference_repo_mount = format!("{}:/ref/repo:ro", repo);
            args.push("-v");
            args.push(&reference_repo_mount);
        }

        // Token env vars
        let token_env_strs: Vec<String> = opts.tokens.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        for token_env in &token_env_strs {
            args.push("-e");
            args.push(token_env);
        }

        args.push(&image_str);
        args.push("sleep");
        args.push("infinity");

        self.inner.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Noop).await?;

        Ok(self.inner.provisioned_environment(id, image.clone(), container_name))
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        let format = r#"{{.Names}}\t{{.Label "flotilla.environment"}}\t{{.Image}}"#;
        let output = self
            .inner
            .runner
            .run("docker", &["ps", "-a", "--filter", "label=flotilla.environment", "--format", format], Path::new("/"), &ChannelLabel::Noop)
            .await?;

        let handles = output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() < 3 {
                    return None;
                }
                let container_name = parts[0].to_string();
                let env_id = parts[1].to_string();
                let image = parts[2].to_string();
                if env_id.is_empty() {
                    return None;
                }
                Some(self.inner.provisioned_environment(EnvironmentId::new(env_id), ImageId::new(image), container_name))
            })
            .collect();

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

    fn provisioned_environment(self: &Arc<Self>, id: EnvironmentId, image: ImageId, container_name: String) -> EnvironmentHandle {
        let runner = Arc::new(DockerEnvironmentRunner::new(container_name.clone(), Arc::clone(&self.runner))) as Arc<dyn CommandRunner>;
        Arc::new(DockerProvisionedEnvironment { id, container_name, image, inner: Arc::clone(self), runner })
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
    inner: Arc<DockerEnvironmentProviderInner>,
    runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl ProvisionedEnvironment for DockerProvisionedEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
    }

    fn container_name(&self) -> Option<&str> {
        Some(&self.container_name)
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
