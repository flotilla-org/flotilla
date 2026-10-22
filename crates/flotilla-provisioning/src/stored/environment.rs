//! Environment: where crews and tools run, realised by one provider instance.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Address, Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    Resource,
};

use super::image_build::ImageBuild;

/// Environments as a lifecycle kind.
pub struct EnvironmentKind;

impl LifecycleKind for EnvironmentKind {
    const NAME: &'static str = "Environment";
    type Request = EnvironmentRequest;
}

/// The Environment machine: the shared lifecycle over its request.
pub type Environment = Lifecycle<EnvironmentKind>;

/// What an Environment asks its provider for. It declares the stored
/// Environment (kind code `ev`), homed at the host whose provider realises it.
///
/// Realised under the provider's identity for it: a Docker container id, or
/// the host-direct root. The image the container runs, its configured limits
/// and its runtime samples are observations.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Environment",
    code = "ev",
    state = LifecycleState,
    index(name = "live_by_provider", eq(provider), when(state in [Requested, Realised])),
    index(name = "by_image_build", eq(image_build), when(image_build is_some))
)]
pub struct EnvironmentRequest {
    /// The provider instance: `host:<host>/provider:<instance>` or
    /// `account:<account>/provider:<instance>` (ADR 0058).
    pub provider: Address,
    /// The ImageBuild that realises a Docker backing's image, when one does.
    /// A column of its own, not part of the backing, so that it is indexed.
    pub image_build: Option<Ref<Resource<ImageBuild>>>,
    pub backing: Backing,
}

/// How the provider backs the environment. Docker's adapter, mount and env
/// rows are child tables that hold rows only under Docker: decoding refuses
/// a row under any other variant.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Backing {
    /// The host itself.
    #[store(code = "hd")]
    HostDirect { repo_default_dir: String },
    /// A container from `image`.
    #[store(code = "dk")]
    Docker {
        image: String,
        /// Adapters the placement policy expects discovery to find in the image.
        #[store(child)]
        declared_adapters: BTreeSet<String>,
        /// Adapters this environment's workflow will launch.
        #[store(child)]
        required_adapters: BTreeSet<String>,
        pull: PullPolicy,
        #[store(flatten)]
        memory: MemoryPolicy,
        #[store(child)]
        mounts: Vec<Mount>,
        #[store(child)]
        env: BTreeMap<String, String>,
    },
}

/// When the provider pulls the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored)]
pub enum PullPolicy {
    #[store(code = "al")]
    Always,
    #[store(code = "ip")]
    IfNotPresent,
    #[store(code = "nv")]
    Never,
}

/// The placement-authored memory budget: a share of host memory per crew.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct MemoryPolicy {
    /// 1 to 100, checked where the request is written (`EnvironmentMemoryPolicy::resolve`).
    pub host_memory_percent: u8,
    pub expected_concurrent_crews: u32,
    /// Swap per crew, in bytes.
    pub swap_bytes: u64,
}

/// One bind mount, in order.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Mount {
    pub source_path: String,
    pub target_path: String,
    pub mode: MountMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored)]
pub enum MountMode {
    #[store(code = "ro")]
    ReadOnly,
    #[store(code = "rw")]
    ReadWrite,
}
