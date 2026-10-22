//! Environment's binding: the old Environment reconciler's patches as lifecycle steps.

use flotilla_resources::{Environment as OldEnvironment, EnvironmentPhase, EnvironmentStatusPatch, ResourceObject, VESSEL_REF_LABEL};
use flotilla_schema::{Frozen, Ref};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome, Resource};

use super::{Binding, Step};
use crate::stored::{
    environment::{
        DockerImage, DockerRecipe, Environment, EnvironmentKind, EnvironmentRequest, HostDirectRecipe, Recipe, LAUNCH_DERIVED_ENV,
    },
    Holder,
};

pub struct EnvironmentBinding;

impl Binding for EnvironmentBinding {
    type Old = OldEnvironment;
    type Kind = EnvironmentKind;

    fn request(obj: &ResourceObject<OldEnvironment>, home: &str) -> Result<EnvironmentRequest, String> {
        let spec = &obj.spec;
        let provider = spec.resolved_provider_address(None)?.ok_or("an environment names no provider or host")?;
        // The provider's own recipe; only the build it names is a column (look-over rulings 5-6).
        let (recipe, image_build) = match (&spec.docker, &spec.host_direct) {
            (Some(docker), None) => {
                // The build is homed at its builder; the environment names it as replicated here.
                let image_build = docker.image_build_ref.as_ref().map(|name| Ref::new(name.clone(), home));
                let recipe = DockerRecipe {
                    // A composed image names its build, whose realised digest is read at creation;
                    // the old spec's empty image is that placeholder.
                    image: match image_build {
                        Some(_) => DockerImage::Built,
                        None => DockerImage::Pinned { reference: docker.image.clone() },
                    },
                    declared_adapters: docker.declared_agent_adapters.clone(),
                    required_adapters: docker.required_agent_adapters.clone(),
                    pull_policy: docker.pull_policy,
                    memory_policy: docker.memory_policy.clone(),
                    mounts: docker.mounts.clone(),
                    // Credential grants and skills are composed at launch, never stored.
                    env: docker
                        .env
                        .iter()
                        .filter(|(key, _)| !LAUNCH_DERIVED_ENV.contains(&key.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                };
                recipe.validate(image_build.as_ref())?;
                (Recipe::of(&recipe)?, image_build)
            }
            (None, Some(host)) => {
                let recipe = HostDirectRecipe { repo_default_dir: host.repo_default_dir.clone() };
                recipe.validate()?;
                (Recipe::of(&recipe)?, None)
            }
            _ => return Err("an environment is backed by exactly one of docker and host_direct".into()),
        };
        let recipe = Frozen::new(recipe).map_err(|error| format!("freeze the provider's recipe: {error}"))?;
        // A crew's environment is held for its vessel, which the old environment names by label.
        // An empty label names no vessel.
        let holder = obj.metadata.labels.get(VESSEL_REF_LABEL).filter(|vessel| !vessel.is_empty()).map(|vessel| Holder::vessel(vessel));
        Ok(EnvironmentRequest { provider, image_build, recipe, holder })
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
