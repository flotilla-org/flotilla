use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ApiPaths, FrozenImageLayer, InputMeta, PlacedImageIdentity, ReplicationClass, ResolvedImageInputs, Resource, ResourceError, StatusPatch,
};

/// One immutable execution of one composition stage. Retry executions refer to
/// their predecessor rather than erasing its failure, inputs or output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBuild;
impl Resource for ImageBuild {
    type Spec = ImageBuildSpec;
    type Status = ImageBuildStatus;
    type StatusPatch = ImageBuildStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "imagebuilds", kind: "ImageBuild" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        if spec.recipe_key != spec.inputs.execution_key(spec.execution_nonce.as_deref()).map_err(ResourceError::invalid)? {
            return Err(ResourceError::invalid("image build recipe key does not match resolved inputs"));
        }
        spec.layer.spec.validate().map_err(ResourceError::invalid)?;
        if spec.host_ref.is_empty() || spec.reservation.cpu == 0 || spec.reservation.disk_bytes == 0 {
            return Err(ResourceError::invalid("image build needs a host and positive CPU and disk reservation"));
        }
        Ok(())
    }

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current != requested {
            return Err(ResourceError::invalid("image build execution inputs are immutable"));
        }
        Ok(())
    }

    fn validate_status_update(current: Option<&Self::Status>, requested: &Self::Status) -> Result<(), ResourceError> {
        if current.is_some_and(|status| matches!(status.phase, ImageBuildPhase::Built | ImageBuildPhase::Failed) && status != requested) {
            return Err(ResourceError::invalid("completed image build execution evidence is immutable"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildSpec {
    pub recipe_key: String,
    pub execution_nonce: Option<String>,
    pub inputs: ResolvedImageInputs,
    pub layer: FrozenImageLayer,
    pub host_ref: String,
    pub reservation: ImageBuildReservation,
    pub parent_build_ref: Option<String>,
    pub previous_build_ref: Option<String>,
    pub attempt: u32,
    pub not_before: Option<DateTime<Utc>>,
    pub reason: ImageBuildReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildReason {
    pub description: String,
    pub old_inputs: BTreeMap<String, String>,
    pub new_inputs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildReservation {
    pub cpu: u32,
    pub disk_bytes: u64,
}

/// An absent declaration preserves placement-host builds for installations
/// predating ImageBuild. Explicit None forbids building on that host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageBuildCapacity {
    None,
    Builder { architecture: String, slots: u32, reservation: ImageBuildReservation },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageBuildPhase {
    #[default]
    Queued,
    Building,
    Built,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageBuildFailureClass {
    Transient,
    Deterministic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBuildFailure {
    pub class: ImageBuildFailureClass,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBuildStatus {
    pub phase: ImageBuildPhase,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub parent_digest: Option<String>,
    pub identity: Option<PlacedImageIdentity>,
    pub verified_provides: BTreeSet<String>,
    pub log_ref: Option<String>,
    pub failure: Option<ImageBuildFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageBuildStatusPatch {
    Start { at: DateTime<Utc>, parent_digest: String },
    Built { at: DateTime<Utc>, identity: PlacedImageIdentity, verified_provides: BTreeSet<String>, log_ref: String },
    Failed { at: DateTime<Utc>, failure: ImageBuildFailure, log_ref: String },
}
impl StatusPatch<ImageBuildStatus> for ImageBuildStatusPatch {
    fn apply(&self, status: &mut ImageBuildStatus) {
        match self {
            Self::Start { at, parent_digest } => {
                status.phase = ImageBuildPhase::Building;
                status.started_at = Some(*at);
                status.parent_digest = Some(parent_digest.clone());
            }
            Self::Built { at, identity, verified_provides, log_ref } => {
                status.phase = ImageBuildPhase::Built;
                status.finished_at = Some(*at);
                status.identity = Some(identity.clone());
                status.verified_provides = verified_provides.clone();
                status.log_ref = Some(log_ref.clone());
            }
            Self::Failed { at, failure, log_ref } => {
                status.phase = ImageBuildPhase::Failed;
                status.finished_at = Some(*at);
                status.failure = Some(failure.clone());
                status.log_ref = Some(log_ref.clone());
            }
        }
    }
}

/// Prefer execution evidence written by the declared build host over the
/// admitting root's queued demand. Never borrow another recipe's old digest.
pub async fn read_image_build(
    backend: &crate::ResourceBackend,
    namespace: &str,
    name: &str,
) -> Result<crate::ResourceObject<ImageBuild>, ResourceError> {
    let mut sources = backend
        .including_replicas::<ImageBuild>(namespace)
        .list()
        .await?
        .items
        .into_iter()
        .filter(|source| source.object.metadata.name == name)
        .collect::<Vec<_>>();
    if let Some(first) = sources.first() {
        if sources.iter().any(|source| {
            source.object.spec.inputs != first.object.spec.inputs || source.object.spec.recipe_key != first.object.spec.recipe_key
        }) {
            return Err(ResourceError::invalid(format!("image build {name} has conflicting immutable sources")));
        }
    }
    sources.sort_by_key(|source| {
        let actor = source
            .object
            .metadata
            .annotations
            .get(crate::ACTUATOR_HOST_REF_ANNOTATION)
            .is_some_and(|host| host == &source.object.spec.host_ref);
        let phase = match source.object.status.as_ref().map_or(ImageBuildPhase::Queued, |status| status.phase) {
            ImageBuildPhase::Built => 3,
            ImageBuildPhase::Failed => 2,
            ImageBuildPhase::Building => 1,
            ImageBuildPhase::Queued => 0,
        };
        (std::cmp::Reverse(actor), std::cmp::Reverse(phase), source.object.metadata.creation_timestamp)
    });
    sources.into_iter().next().map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageInputStability;

    // #2271: unknown external inputs have no shareable key. Identical execution
    // evidence is stable, but every distinct nonce (including host identity)
    // makes a distinct non-shareable key. Generate empty and boundary nonces.
    #[hegel::test]
    fn unpinned_executions_cannot_claim_shared_identity(tc: hegel::TestCase) {
        let nonce = tc.draw(hegel::generators::integers::<u64>().min_value(0).max_value(u64::MAX));
        let inputs = ResolvedImageInputs::builder()
            .parent_digest(format!("sha256:{}", "1".repeat(64)))
            .content_hashes(vec![format!("sha256:{}", "2".repeat(64))])
            .architecture("amd64".into())
            .stability(ImageInputStability::Unpinned)
            .build();
        assert!(inputs.recipe_key().is_err());
        assert!(inputs.execution_key(None).is_err());
        assert!(inputs.execution_key(Some("")).is_err());
        let first = format!("host-a:{nonce}");
        let second = format!("host-b:{nonce}");
        let key = inputs.execution_key(Some(&first)).expect("execution key");
        assert_eq!(key, inputs.execution_key(Some(&first)).expect("repeated execution"));
        assert_ne!(key, inputs.execution_key(Some(&second)).expect("different execution"));
    }
}
