//! Realisation settings, held as code constants rather than stored.

use crate::{CloudSandboxSize, DockerImagePullPolicy};

/// Where a Docker vessel's checkout is mounted, and its working directory.
pub const DOCKER_WORKSPACE: &str = "/workspace";

pub const DOCKER_PULL_POLICY: DockerImagePullPolicy = DockerImagePullPolicy::Always;

/// The image architecture cloud sandboxes run: drydock's guests are linux/amd64.
pub const CLOUD_SANDBOX_ARCHITECTURE: &str = "amd64";

/// The size a cloud sandbox is provisioned at when its kind names none.
pub const CLOUD_SANDBOX_SIZE: CloudSandboxSize = CloudSandboxSize { cpus: 2, memory_bytes: 4 << 30, disk_bytes: 10 << 30 };
