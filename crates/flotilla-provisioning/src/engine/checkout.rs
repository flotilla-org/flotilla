//! Checkout's binding: the old Checkout reconciler's patches as lifecycle steps.

use flotilla_resources::{Checkout as OldCheckout, CheckoutPhase, CheckoutSpec, CheckoutStatusPatch, ResourceObject, CONVOY_LABEL};
use flotilla_schema::Ref;
use flotilla_state_machines::{Cause, CauseCategory, Outcome};

use super::{Binding, Step};
use crate::stored::{
    checkout::{CheckoutKind, CheckoutRequest, Source},
    Holder,
};

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
        // Held for its convoy, when a convoy role's: the old checkout names it by label. The
        // holder carries no home and no ownership: the convoy finds its checkouts by it. An empty
        // label names no convoy.
        let holder = obj.metadata.labels.get(CONVOY_LABEL).filter(|convoy| !convoy.is_empty()).map(|convoy| Holder::convoy(convoy));
        let request = CheckoutRequest {
            repository: obj.spec.repo_ref().to_string(),
            environment: local(environment, home),
            branch: branch.clone(),
            base: base.clone(),
            path: path.clone(),
            source,
            clone,
            holder,
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
            CheckoutStatusPatch::MarkGone => {
                vec![Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Lost }]
            }
            CheckoutStatusPatch::ObserveDegradation { .. }
            | CheckoutStatusPatch::ObserveCloneRetry { .. }
            | CheckoutStatusPatch::ObserveValidation { .. }
            | CheckoutStatusPatch::UpdateIntegration { .. } => vec![],
        }
    }
}

fn phase(obj: &ResourceObject<OldCheckout>) -> CheckoutPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
