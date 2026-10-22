//! Checkout's binding: the old Checkout reconciler's patches as lifecycle steps.

use flotilla_resources::{
    Checkout as OldCheckout, CheckoutPhase, CheckoutSpec, CheckoutStatusPatch, ResourceObject, ACTUATOR_SOURCE_ROOT_ANNOTATION,
    CONVOY_LABEL,
};
use flotilla_schema::Ref;
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome, Resource};

use super::{Binding, Step};
use crate::stored::checkout::{Checkout, CheckoutKind, CheckoutRequest, Source};

pub struct CheckoutBinding;

impl Binding for CheckoutBinding {
    type Old = OldCheckout;
    type Kind = CheckoutKind;

    fn request(obj: &ResourceObject<OldCheckout>, home: &str) -> Result<CheckoutRequest, String> {
        fn local<K>(name: &str, home: &str) -> Ref<K> {
            Ref::new(name.to_owned(), home)
        }
        let (environment, branch, base, path, source, clone) = match &obj.spec {
            CheckoutSpec::Worktree(spec) => {
                (&spec.env_ref, &spec.r#ref, &spec.base_ref, &spec.target_path, Source::Worktree, Some(local(&spec.clone_ref, home)))
            }
            CheckoutSpec::FreshClone(spec) => (
                &spec.env_ref,
                &spec.r#ref,
                &spec.base_ref,
                &spec.target_path,
                Source::FreshClone { url: spec.url.clone() },
                spec.source_clone_ref.as_deref().map(|name| local(name, home)),
            ),
            CheckoutSpec::Observed(_) => return Err("an observed checkout is no engine request".into()),
        };
        // The convoy is homed where it was admitted: the root a federated convoy's checkout
        // records, or this home.
        let convoy = obj.metadata.labels.get(CONVOY_LABEL).map(|convoy| {
            let convoy_home = obj.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map_or(home, String::as_str);
            Ref::new(convoy.clone(), convoy_home)
        });
        let request = CheckoutRequest {
            repository: obj.spec.repo_ref().to_string(),
            environment: local(environment, home),
            branch: branch.clone(),
            base: base.clone(),
            path: path.clone(),
            source,
            clone,
            convoy,
            ensure: None,
        };
        request.validate()?;
        Ok(request)
    }

    fn initial(obj: &ResourceObject<OldCheckout>) -> bool {
        phase(obj) == CheckoutPhase::Pending
    }

    fn steps(_obj: &ResourceObject<OldCheckout>, patch: &CheckoutStatusPatch) -> Vec<Step> {
        match patch {
            // Realised under its path, where the branch was observed checked out.
            CheckoutStatusPatch::MarkReady { path, .. } => vec![Step::Realised { identity: path.clone() }],
            // A refused branch, a failed clone, a creation the reconciler does not retry.
            CheckoutStatusPatch::MarkFailed { message } => {
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Refused, message), retryable: false })]
            }
            // A Ready checkout observed absent on its host. A Gone one observed again is a
            // replacement for its owner to request: the engine has no incarnations.
            CheckoutStatusPatch::MarkGone => vec![Step::Vanished],
            CheckoutStatusPatch::ObserveDegradation { .. }
            | CheckoutStatusPatch::ObserveCloneRetry { .. }
            | CheckoutStatusPatch::ObserveValidation { .. }
            | CheckoutStatusPatch::UpdateIntegration { .. } => vec![],
        }
    }

    fn copy(obj: &ResourceObject<OldCheckout>, resource: &Resource<Checkout>) -> Option<CheckoutStatusPatch> {
        let status = obj.status.as_ref();
        match (resource.state(), phase(obj)) {
            // The old shape has no patch back to Pending; a Requested checkout is never past it.
            (LifecycleState::Requested { .. }, _) => None,
            (LifecycleState::Realised { .. }, CheckoutPhase::Ready) => None,
            (LifecycleState::Realised { identity }, _) => Some(CheckoutStatusPatch::MarkReady {
                path: identity.clone(),
                commit: status.and_then(|status| status.commit.clone()),
                branch_provenance: status.map(|status| status.branch_provenance).unwrap_or_default(),
            }),
            (LifecycleState::Gone, CheckoutPhase::Gone) => None,
            (LifecycleState::Gone, _) => Some(CheckoutStatusPatch::MarkGone),
            (LifecycleState::GaveUp { .. }, CheckoutPhase::Failed) => None,
            (LifecycleState::GaveUp { cause }, _) => Some(CheckoutStatusPatch::MarkFailed { message: cause.detail.clone() }),
        }
    }
}

fn phase(obj: &ResourceObject<OldCheckout>) -> CheckoutPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
