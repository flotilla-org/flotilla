//! ImageBuild's binding: the old ImageBuild reconciler's patches as lifecycle steps.

use flotilla_resources::{
    ImageBuild as OldImageBuild, ImageBuildFailureClass, ImageBuildPhase, ImageBuildStatusPatch, ImageDelivery, ResourceObject,
};
use flotilla_schema::Frozen;
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome, Resource};

use super::{Binding, Step};
use crate::stored::image_build::{Architecture, Delivery, ImageBuild, ImageBuildKind, ImageBuildRequest, Reservation};

pub struct ImageBuildBinding;

impl Binding for ImageBuildBinding {
    type Old = OldImageBuild;
    type Kind = ImageBuildKind;

    fn request(obj: &ResourceObject<OldImageBuild>, _home: &str) -> Result<ImageBuildRequest, String> {
        let spec = obj.spec.current().ok_or("a retired per-stage image build is no engine request")?;
        Ok(ImageBuildRequest {
            composition: spec.identity.clone(),
            architecture: match spec.architecture.as_str() {
                "amd64" => Architecture::Amd64,
                "arm64" => Architecture::Arm64,
                other => return Err(format!("image build architecture `{other}` must be amd64 or arm64")),
            },
            layers: Frozen::new(spec.layers.clone()).map_err(|error| format!("freeze the layers: {error}"))?,
            delivery: match &spec.delivery {
                ImageDelivery::Registry { repository } => Delivery::Registry { repository: repository.clone() },
                ImageDelivery::Local => Delivery::Local,
            },
            reservation: Reservation { cpu: spec.reservation.cpu, disk_bytes: spec.reservation.disk_bytes },
        })
    }

    fn initial(obj: &ResourceObject<OldImageBuild>) -> bool {
        // Building is an attempt in flight: still Requested.
        matches!(phase(obj), ImageBuildPhase::Queued | ImageBuildPhase::Building)
    }

    fn steps(_obj: &ResourceObject<OldImageBuild>, patch: &ImageBuildStatusPatch) -> Vec<Step> {
        match patch {
            // Realised under the built image's digest.
            ImageBuildStatusPatch::Built { digest, .. } => vec![Step::Realised { identity: digest.clone() }],
            // An infrastructure failure is retried while the policy allows; a recipe never succeeds on the same inputs.
            ImageBuildStatusPatch::Failed { failure, .. } => {
                let (category, retryable) = match failure.class {
                    ImageBuildFailureClass::Infrastructure => (CauseCategory::Transient, true),
                    ImageBuildFailureClass::Recipe => (CauseCategory::Refused, false),
                };
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(category, &failure.reason), retryable })]
            }
            // Starting an attempt and waiting out its backoff are judgements over the tally.
            ImageBuildStatusPatch::Start { .. } | ImageBuildStatusPatch::Retry { .. } => vec![],
        }
    }

    // Remove this copy once the legacy validator no longer requires copied lifecycle status.
    fn copy(obj: &ResourceObject<OldImageBuild>, resource: &Resource<ImageBuild>) -> Option<ImageBuildStatusPatch> {
        let status = obj.status.clone().unwrap_or_default();
        // A stored time outside chrono's range cannot occur for a write made now; the epoch stands in.
        let at = resource.since().to_datetime().unwrap_or_default();
        match resource.state() {
            // Every old phase short of Built is consistent with a request still being tried:
            // a failure the engine has not given up on is retried.
            LifecycleState::Requested { .. } if status.phase != ImageBuildPhase::Built => None,
            // Built in the old status alone (a refused realise): the engine still wants it, so the
            // build is queued again. A resource is never realised twice, so this needs a refusal.
            LifecycleState::Requested { .. } => Some(ImageBuildStatusPatch::Retry { not_before: at }),
            LifecycleState::Realised { .. } if status.phase == ImageBuildPhase::Built => None,
            LifecycleState::Realised { identity } => Some(ImageBuildStatusPatch::Built {
                at,
                digest: identity.clone(),
                probes: status.probes.clone(),
                log_ref: status.log_ref.clone().unwrap_or_default(),
            }),
            // Settled failed: no retry is to come.
            LifecycleState::GaveUp { .. } if status.phase == ImageBuildPhase::Failed && !status.will_retry() => None,
            LifecycleState::GaveUp { cause } => Some(ImageBuildStatusPatch::Failed {
                at,
                failure: flotilla_resources::ImageBuildFailure {
                    // Recipe: the old shape's terminal class, so nothing requeues it. The real class
                    // stays on the attempt's log entry, where the import (layer 9) reads it.
                    class: ImageBuildFailureClass::Recipe,
                    reason: cause.detail.clone(),
                },
                probes: status.probes.clone(),
                log_ref: status.log_ref.clone().unwrap_or_default(),
            }),
            // An image build is released only as its record is deleted.
            LifecycleState::Gone { .. } => None,
        }
    }
}

fn phase(obj: &ResourceObject<OldImageBuild>) -> ImageBuildPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
