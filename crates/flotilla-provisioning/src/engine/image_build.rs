//! ImageBuild's binding: the old ImageBuild reconciler's patches as lifecycle steps.

use flotilla_resources::{
    ImageBuild as OldImageBuild, ImageBuildFailureClass, ImageBuildPhase, ImageBuildStatusPatch, ImageDelivery, ResourceObject,
};
use flotilla_schema::Frozen;
use flotilla_state_machines::{Cause, CauseCategory, Outcome};

use super::{Binding, Step};
use crate::stored::image_build::{Architecture, Delivery, ImageBuildKind, ImageBuildRequest, Reservation};

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

    // Only consulted before an engine request; an existing lifecycle never selects its phase here.
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
}

fn phase(obj: &ResourceObject<OldImageBuild>) -> ImageBuildPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
