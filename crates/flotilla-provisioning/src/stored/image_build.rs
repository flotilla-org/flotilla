//! ImageBuild: one build of one composed image identity for one architecture (ADR 0062).

use flotilla_resources::{ImageLayerSpec, IMAGE_BUILD_MAX_ATTEMPTS};
use flotilla_schema::{Frozen, IndexQuery, Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    AttemptPolicy,
};

use super::{host::HostRecord, SECOND};

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
    fn launch(request: &ImageBuildRequest) -> Option<(&str, u64)> {
        Some((&request.launch_of, request.launch))
    }
    fn numbered(request: ImageBuildRequest, launch: u64) -> ImageBuildRequest {
        ImageBuildRequest { launch, ..request }
    }
}

impl super::Launches for ImageBuildKind {
    fn by_launch(of: &str) -> IndexQuery<ImageBuildRequest> {
        ImageBuildRequest::by_launch(&of.to_owned())
    }
}

/// The ImageBuild machine: the shared lifecycle over its request.
pub type ImageBuild = Lifecycle<ImageBuildKind>;

/// What an ImageBuild builds. It declares the stored ImageBuild (kind code
/// `ib`), homed at its builder host, so the builder is the resource's home
/// and no column of its own. The builder is its one writer: it requests a
/// launch for the [`ImageBuildDemand`]s naming it, one launch serving every
/// demand for the same build at every home.
///
/// Each launch is named `<build>#<n>`, the build named by
/// `image_build_name`, so a build that gave up is launched again under its
/// own name on the operator's retry, never revived.
///
/// Realised under the built image's digest: `<repository>@sha256:…` for
/// registry delivery, the `sha256:…` image id for a local one. Each try is an
/// attempt; an infrastructure failure is retryable and a recipe failure is
/// not, so the lifecycle's tally and policy replace the old attempt counter
/// and `not_before`. Layer probes and the build log are evidence in the
/// realising or attempt edge's log entry.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "ImageBuild",
    code = "ib",
    state = LifecycleState,
    index(name = "by_composition", eq(composition)),
    // A build's launches in launch order: the newest is the last.
    index(name = "by_launch", eq(launch_of), order(launch))
)]
pub struct ImageBuildRequest {
    /// The build this is a launch of (`image_build_name`), interned (domain `ln`).
    #[store(intern = "ln")]
    pub launch_of: String,
    /// The home sequence the launch's create took; 0 until its log numbers it.
    pub launch: u64,
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

/// An admitting home's demand for a build: written once by admission at its home, its one writer,
/// and deleted with an abandoned admission. Its builder follows the admitting home's log, finds
/// the demands naming it through `by_builder`, and launches the [`ImageBuildRequest`] they ask
/// for. Named by `image_build_name`, as the build it asks for.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ImageBuildDemand", code = "iq", index(name = "by_builder", eq(builder)), index(name = "by_composition", eq(composition)))]
pub struct ImageBuildDemand {
    /// The composed identity, `image_identity` of the layers: `sha256:<hex>`.
    pub composition: String,
    pub architecture: Architecture,
    /// Shared by content hash with the builds and other demands of the same layers.
    #[store(frozen)]
    pub layers: Frozen<Vec<ImageLayerSpec>>,
    /// The builder's Host record, homed at the builder: the home its build is launched at.
    pub builder: Ref<HostRecord>,
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
