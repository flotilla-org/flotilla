//! Docker-backed environment provider.
//!
//! Shells out to the `docker` CLI via `CommandRunner`, consistent with how every
//! other provider interacts with external tools.

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
use uuid::Uuid;

use super::{runner::EnvironmentRunner, CreateOpts, EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment};
use crate::providers::{ChannelLabel, CommandRunner};

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
    async fn ensure_image(&self, spec: &EnvironmentSpec) -> Result<ImageId, String> {
        match &spec.image {
            ImageSource::Dockerfile(path) => {
                let tag = format!("flotilla-env-{}", Uuid::new_v4());
                let context_dir = path.parent().unwrap_or(Path::new(".")).to_string_lossy().into_owned();
                let path_str = path.to_string_lossy().into_owned();
                self.runner
                    .run("docker", &["build", "-t", &tag, "-f", &path_str, &context_dir], Path::new("/"), &ChannelLabel::Noop)
                    .await?;
                Ok(ImageId::new(tag))
            }
            ImageSource::Registry(image) => {
                self.runner.run("docker", &["pull", image], Path::new("/"), &ChannelLabel::Noop).await?;
                Ok(ImageId::new(image.clone()))
            }
        }
    }

    async fn create(&self, image: &ImageId, opts: CreateOpts) -> Result<EnvironmentHandle, String> {
        let id = EnvironmentId::new(Uuid::new_v4().to_string());
        let container_name = format!("flotilla-env-{}", id);

        let socket_str = opts.daemon_socket_path.to_string();
        let env_id_str = id.to_string();
        let image_str = image.as_str().to_string();
        let label_val = format!("flotilla.environment={}", id);
        let socket_mount = format!("{}:/run/flotilla.sock", socket_str);
        let env_id_env = format!("FLOTILLA_ENVIRONMENT_ID={}", env_id_str);

        let mut args = vec![
            "run",
            "-d",
            "--name",
            &container_name,
            "--label",
            &label_val,
            "-v",
            &socket_mount,
            "-e",
            "FLOTILLA_DAEMON_SOCKET=/run/flotilla.sock",
            "-e",
            &env_id_env,
        ];

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

        self.runner.run("docker", &args, Path::new("/"), &ChannelLabel::Noop).await?;

        Ok(Arc::new(DockerProvisionedEnvironment { id, container_name, image: image.clone(), runner: self.runner.clone() }))
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        let format = r#"{{.Names}}\t{{.Label "flotilla.environment"}}\t{{.Image}}"#;
        let output = self
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
                Some(Arc::new(DockerProvisionedEnvironment {
                    id: EnvironmentId::new(env_id),
                    container_name,
                    image: ImageId::new(image),
                    runner: self.runner.clone(),
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
}

#[async_trait]
impl ProvisionedEnvironment for DockerProvisionedEnvironment {
    fn id(&self) -> &EnvironmentId {
        &self.id
    }

    fn image(&self) -> &ImageId {
        &self.image
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
