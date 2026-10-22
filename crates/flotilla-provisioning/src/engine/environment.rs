//! Environment's binding: the old Environment reconciler's patches as lifecycle steps.

use flotilla_resources::{Environment as OldEnvironment, EnvironmentPhase, EnvironmentStatusPatch, ResourceObject};
use flotilla_schema::{Frozen, Ref};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome, Resource};

use super::{Binding, Step};
use crate::stored::environment::{Environment, EnvironmentKind, EnvironmentRequest, Recipe};

pub struct EnvironmentBinding;

impl Binding for EnvironmentBinding {
    type Old = OldEnvironment;
    type Kind = EnvironmentKind;

    fn request(obj: &ResourceObject<OldEnvironment>, home: &str) -> Result<EnvironmentRequest, String> {
        let spec = &obj.spec;
        let provider = spec.resolved_provider_address(None)?.ok_or("an environment names no provider or host")?;
        // The provider's own spec is its recipe; only the build it names is a column.
        let (recipe, image_build) = match (&spec.docker, &spec.host_direct) {
            // The build is homed at its builder; the environment names it as replicated here.
            (Some(docker), None) => (Recipe::of(docker)?, docker.image_build_ref.as_ref().map(|name| Ref::new(name.clone(), home))),
            (None, Some(host)) => (Recipe::of(host)?, None),
            _ => return Err("an environment is backed by exactly one of docker and host_direct".into()),
        };
        let recipe = Frozen::new(recipe).map_err(|error| format!("freeze the provider's recipe: {error}"))?;
        Ok(EnvironmentRequest { provider, image_build, recipe })
    }

    fn initial(obj: &ResourceObject<OldEnvironment>) -> bool {
        matches!(phase(obj), EnvironmentPhase::Pending | EnvironmentPhase::Provisioning)
    }

    fn steps(obj: &ResourceObject<OldEnvironment>, patch: &EnvironmentStatusPatch) -> Vec<Step> {
        match patch {
            // Realised under the provider's identity: the container, or the host-direct root.
            EnvironmentStatusPatch::MarkReady { docker_container_id, .. } => {
                vec![Step::Realised { identity: identity(obj, docker_container_id.as_deref()) }]
            }
            // A realised environment whose backing failed is no longer there.
            EnvironmentStatusPatch::MarkFailed { .. } if phase(obj) == EnvironmentPhase::Ready => vec![Step::Vanished],
            // The provider refused it: the old environment never retried a failure.
            EnvironmentStatusPatch::MarkFailed { message } => {
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, message), retryable: false })]
            }
            EnvironmentStatusPatch::MarkLost { .. } => vec![Step::Vanished],
            EnvironmentStatusPatch::WaitForImageBuild { .. }
            | EnvironmentStatusPatch::ObserveRuntime { .. }
            | EnvironmentStatusPatch::CredentialDelivery { .. }
            | EnvironmentStatusPatch::CredentialRefresh { .. } => vec![],
        }
    }

    fn copy(obj: &ResourceObject<OldEnvironment>, resource: &Resource<Environment>) -> Option<EnvironmentStatusPatch> {
        let status = obj.status.as_ref();
        let message = || status.and_then(|status| status.message.clone()).unwrap_or_default();
        match (resource.state(), phase(obj)) {
            (LifecycleState::Requested { .. }, EnvironmentPhase::Pending | EnvironmentPhase::Provisioning) => None,
            (LifecycleState::Requested { .. }, _) => Some(EnvironmentStatusPatch::WaitForImageBuild { message: message() }),
            (LifecycleState::Realised { .. }, EnvironmentPhase::Ready) => None,
            (LifecycleState::Realised { identity }, _) => Some(EnvironmentStatusPatch::MarkReady {
                configured_limits: status.and_then(|status| status.configured_limits.clone()),
                docker_container_id: obj.spec.docker.is_some().then(|| identity.clone()),
                image_ref: status.and_then(|status| status.image_ref.clone()),
                local_image_id: status.and_then(|status| status.local_image_id.clone()),
                registry_digest: status.and_then(|status| status.registry_digest.clone()),
            }),
            (LifecycleState::Gone, EnvironmentPhase::Lost | EnvironmentPhase::Failed) => None,
            // The old message, when it has one, says why: the copy keeps it.
            (LifecycleState::Gone, _) => Some(EnvironmentStatusPatch::MarkLost {
                message: status.and_then(|status| status.message.clone()).unwrap_or_else(|| "the environment is gone".into()),
            }),
            (LifecycleState::GaveUp { .. }, EnvironmentPhase::Failed) => None,
            (LifecycleState::GaveUp { cause }, _) => Some(EnvironmentStatusPatch::MarkFailed { message: cause.detail.clone() }),
        }
    }
}

/// The identity an environment is realised under: its container, or the host-direct root.
fn identity(obj: &ResourceObject<OldEnvironment>, container: Option<&str>) -> String {
    match (container, &obj.spec.host_direct) {
        (Some(container), _) => container.to_owned(),
        (None, Some(host)) => host.repo_default_dir.clone(),
        (None, None) => obj.metadata.name.clone(),
    }
}

fn phase(obj: &ResourceObject<OldEnvironment>) -> EnvironmentPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
