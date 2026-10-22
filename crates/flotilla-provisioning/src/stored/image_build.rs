//! ImageBuild: one build of one composed image identity for one architecture (ADR 0062).

use flotilla_resources::{ImageLayerSpec, IMAGE_BUILD_MAX_ATTEMPTS};
use flotilla_schema::{Frozen, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    AttemptPolicy,
};

use super::SECOND;

/// Image builds as a lifecycle kind.
pub struct ImageBuildKind;

impl LifecycleKind for ImageBuildKind {
    const NAME: &'static str = "ImageBuild";
    type Request = ImageBuildRequest;
    /// The old build's: at most five attempts in all (a build never succeeds
    /// and retries, so its failures are all consecutive), 30 s doubling to 30
    /// minutes between them. A recipe failure is not retryable and gives up at once.
    fn policy() -> AttemptPolicy {
        AttemptPolicy { max_failures: IMAGE_BUILD_MAX_ATTEMPTS, base: 30 * SECOND, factor: 2, cap: 30 * 60 * SECOND }
    }
}

/// The ImageBuild machine: the shared lifecycle over its request.
pub type ImageBuild = Lifecycle<ImageBuildKind>;

/// What an ImageBuild builds. It declares the stored ImageBuild (kind code
/// `ib`), homed at its builder host, so the builder is the resource's home
/// and no column of its own.
///
/// Realised under the built image's digest: `<repository>@sha256:…` for
/// registry delivery, the `sha256:…` image id for a local one. Each try is an
/// attempt; an infrastructure failure is retryable and a recipe failure is
/// not, so the lifecycle's tally and policy replace the old attempt counter
/// and `not_before`. Layer probes and the build log are evidence in the
/// realising or attempt edge's log entry.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ImageBuild", code = "ib", state = LifecycleState, index(name = "by_composition", eq(composition)))]
pub struct ImageBuildRequest {
    /// The composed identity, `image_identity` of the layers: `sha256:<hex>`.
    /// Not `identity`, which is the lifecycle's realised identity.
    pub composition: String,
    pub architecture: Architecture,
    /// The ordered composition, stored once by content hash and shared by
    /// every build of the same layers (on every architecture and host).
    #[store(frozen)]
    pub layers: Frozen<Vec<ImageLayerSpec>>,
    pub delivery: Delivery,
    #[store(flatten)]
    pub reservation: Reservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored)]
pub enum Architecture {
    #[store(code = "a6")]
    Amd64,
    #[store(code = "r6")]
    Arm64,
}

/// Where a built image goes.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Delivery {
    /// Pushed to `repository`, named without tag or digest.
    #[store(code = "rg")]
    Registry { repository: String },
    /// Kept on the builder: single-host mode without a registry.
    #[store(code = "lo")]
    Local,
}

/// The builder capacity the build holds while it runs.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Reservation {
    pub cpu: u32,
    pub disk_bytes: u64,
}
