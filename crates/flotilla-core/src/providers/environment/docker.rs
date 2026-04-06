//! Docker-backed environment provider.
//!
//! Shells out to the `docker` CLI via `CommandRunner`, consistent with how every
//! other provider interacts with external tools.

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
use uuid::Uuid;

use super::{runner::EnvironmentRunner, CreateOpts, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment, ProvisionedMount};
use crate::providers::{ChannelLabel, CommandRunner};

/// Fixed path inside the container where the daemon socket is mounted.
const CONTAINER_SOCKET_PATH: &str = "/run/flotilla.sock";

// ---------------------------------------------------------------------------
// DockerEnvironment
// ---------------------------------------------------------------------------

/// An `EnvironmentProvider` that manages Docker containers as sandbox environments.
pub struct DockerEnvironment {
    runner: Arc<dyn CommandRunner>,
}

impl DockerEnvironment {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl EnvironmentProvider for DockerEnvironment {
    // TODO: images built from Dockerfiles accumulate as `flotilla-env-{uuid}` tags.
    // destroy() removes the container but not the image. Phase D should add image
    // lifecycle management (prune unused flotilla images, or reuse by content hash).
    async fn ensure_image(&self, spec: &EnvironmentSpec, repo_root: &Path) -> Result<ImageId, String> {
        match &spec.image {
            ImageSource::Dockerfile(path) => {
                let tag = format!("flotilla-env-{}", Uuid::new_v4());
                let abs_path = if path.is_relative() { repo_root.join(path) } else { path.clone() };
                let context_dir = abs_path.parent().unwrap_or(repo_root).to_string_lossy().into_owned();
                let path_str = abs_path.to_string_lossy().into_owned();
                self.runner.run("docker", &["build", "-t", &tag, "-f", &path_str, &context_dir], repo_root, &ChannelLabel::Noop).await?;
                Ok(ImageId::new(tag))
            }
            ImageSource::Registry(image) => {
                self.runner.run("docker", &["pull", image], repo_root, &ChannelLabel::Noop).await?;
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
        let mounts_label_val =
            format!("flotilla.provisioned_mounts={}", serde_json::to_string(&opts.provisioned_mounts).map_err(|err| err.to_string())?);
        let socket_mount = format!("{}:{CONTAINER_SOCKET_PATH}", socket_str);
        let env_id_env = format!("FLOTILLA_ENVIRONMENT_ID={}", env_id_str);
        let socket_env = format!("FLOTILLA_DAEMON_SOCKET={CONTAINER_SOCKET_PATH}");

        let mut args = vec![
            "run",
            "-d",
            "--name",
            &container_name,
            "--label",
            &label_val,
            "--label",
            &mounts_label_val,
            "-v",
            &socket_mount,
            "-e",
            &socket_env,
            "-e",
            &env_id_env,
        ];

        let mount_specs: Vec<String> =
            opts.provisioned_mounts.iter().map(|mount| format!("{}:{}:ro", mount.host_path, mount.environment_path)).collect();
        for mount_spec in &mount_specs {
            args.push("-v");
            args.push(mount_spec);
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

        self.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Noop).await?;

        Ok(Arc::new(DockerProvisionedEnvironment {
            id,
            container_name,
            image: image.clone(),
            runner: self.runner.clone(),
            provisioned_mounts: opts.provisioned_mounts,
        }))
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        let format = r#"{{.Names}}\t{{.Label "flotilla.environment"}}\t{{.Image}}\t{{.Label "flotilla.provisioned_mounts"}}"#;
        let output = self
            .runner
            .run("docker", &["ps", "-a", "--filter", "label=flotilla.environment", "--format", format], Path::new("/"), &ChannelLabel::Noop)
            .await?;

        let handles = output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() < 4 {
                    return None;
                }
                let container_name = parts[0].to_string();
                let env_id = parts[1].to_string();
                let image = parts[2].to_string();
                if env_id.is_empty() {
                    return None;
                }
                let provisioned_mounts = match serde_json::from_str(parts[3]) {
                    Ok(mounts) => mounts,
                    Err(err) => {
                        tracing::warn!(container = %container_name, err = %err, raw = %parts[3], "failed to parse provisioned mount metadata; ignoring mount metadata");
                        vec![]
                    }
                };
                Some(Arc::new(DockerProvisionedEnvironment {
                    id: EnvironmentId::new(env_id),
                    container_name,
                    image: ImageId::new(image),
                    runner: self.runner.clone(),
                    provisioned_mounts,
                }) as EnvironmentHandle)
            })
            .collect();

        Ok(handles)
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

    fn container_name(&self) -> Option<&str> {
        Some(&self.container_name)
    }

    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        self.provisioned_mounts.clone()
    }

    async fn status(&self) -> Result<EnvironmentStatus, String> {
        let raw = self
            .runner
            .run("docker", &["inspect", "--format", "{{.State.Status}}", &self.container_name], Path::new("/"), &ChannelLabel::Noop)
            .await?;
        let status = raw.trim();
        Ok(match status {
            "running" => EnvironmentStatus::Running,
            "created" | "restarting" => EnvironmentStatus::Starting,
            "paused" | "exited" | "dead" => EnvironmentStatus::Stopped,
            other => EnvironmentStatus::Failed(other.to_string()),
        })
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        let output =
            self.runner.run("docker", &["exec", &self.container_name, "sh", "-lc", "env"], Path::new("/"), &ChannelLabel::Noop).await?;

        // Note: `sh -lc env` output is line-delimited. Values containing newlines
        // (e.g. PEM certificates) will be silently truncated. Acceptable for Phase 1;
        // a structured query (docker inspect) could provide the full picture if needed.
        let vars = output
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect();

        Ok(vars)
    }

    fn runner(&self, host_runner: Arc<dyn CommandRunner>) -> Arc<dyn CommandRunner> {
        Arc::new(EnvironmentRunner::new(self.container_name.clone(), host_runner))
    }

    async fn destroy(&self) -> Result<(), String> {
        self.runner.run("docker", &["rm", "-f", &self.container_name], Path::new("/"), &ChannelLabel::Noop).await?;
        Ok(())
    }
}
