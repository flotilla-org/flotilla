//! Clone's binding: the old Clone reconciler's patches as lifecycle steps.

use flotilla_resources::{Clone as OldClone, ClonePhase, CloneStatusPatch, ResourceObject};
use flotilla_schema::Ref;
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome, Resource};

use super::{Binding, Step};
use crate::stored::clone::{Clone, CloneKind, CloneRequest};

pub struct CloneBinding;

impl Binding for CloneBinding {
    type Old = OldClone;
    type Kind = CloneKind;

    fn request(obj: &ResourceObject<OldClone>, home: &str) -> Result<CloneRequest, String> {
        let spec = &obj.spec;
        Ok(CloneRequest {
            repository: spec.repo_ref.to_string(),
            url: spec.url.clone(),
            environment: Ref::new(spec.env_ref.clone(), home),
            path: spec.path.clone(),
        })
    }

    fn initial(obj: &ResourceObject<OldClone>) -> bool {
        matches!(phase(obj), ClonePhase::Pending | ClonePhase::Cloning)
    }

    fn steps(obj: &ResourceObject<OldClone>, patch: &CloneStatusPatch) -> Vec<Step> {
        match patch {
            CloneStatusPatch::MarkCloning => vec![],
            // Realised under its path, where the repository was observed.
            CloneStatusPatch::MarkReady { .. } => vec![Step::Realised { identity: obj.spec.path.clone() }],
            CloneStatusPatch::MarkRetrying { message } => {
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, message), retryable: true })]
            }
            // A missing or mismatched repository, or a misnamed clone: its inputs are wrong.
            CloneStatusPatch::MarkFailed { message, .. } => {
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Misconfigured, message), retryable: false })]
            }
        }
    }

    fn copy(obj: &ResourceObject<OldClone>, resource: &Resource<Clone>) -> Option<CloneStatusPatch> {
        let status = obj.status.as_ref();
        match (resource.state(), phase(obj)) {
            (LifecycleState::Requested { .. }, ClonePhase::Pending | ClonePhase::Cloning) => None,
            (LifecycleState::Requested { last_failure, .. }, _) => Some(match last_failure {
                Some(failure) => CloneStatusPatch::MarkRetrying { message: failure.cause.detail.clone() },
                None => CloneStatusPatch::MarkCloning,
            }),
            (LifecycleState::Realised { .. }, ClonePhase::Ready) => None,
            (LifecycleState::Realised { .. }, _) => {
                Some(CloneStatusPatch::MarkReady { default_branch: status.and_then(|status| status.default_branch.clone()) })
            }
            (LifecycleState::GaveUp { .. }, ClonePhase::Failed) if status.is_some_and(|status| status.failure_policy.is_some()) => None,
            (LifecycleState::GaveUp { cause }, _) => Some(CloneStatusPatch::MarkFailed {
                message: cause.detail.clone(),
                failed_at: resource.since().to_datetime().unwrap_or_default(),
            }),
            // A clone is released only as its object is deleted: nothing to copy.
            (LifecycleState::Gone, _) => None,
        }
    }
}

fn phase(obj: &ResourceObject<OldClone>) -> ClonePhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
