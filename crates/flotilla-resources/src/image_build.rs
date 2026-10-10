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
        if current.is_some_and(|status| {
            matches!(status.phase, ImageBuildPhase::Built | ImageBuildPhase::Failed) && {
                let mut evidence = requested.clone();
                evidence.availability = status.availability.clone();
                status != &evidence
            }
        }) {
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
    /// Mutable observations are separate from immutable execution evidence.
    /// ADR 0047: previous records omit this; retain default for one roll.
    #[serde(default)]
    pub availability: ImageAvailability,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ImageAvailabilityRecord")]
pub struct ImageAvailability {
    #[serde(default)]
    pub caches: BTreeSet<LocalImageCacheKey>,
    /// Repository@manifest digest; local config IDs are never substituted.
    pub registry_ref: Option<String>,
    pub failure: Option<String>,
}

// ADR 0047: accept and drop host-only availability for one roll after B1.
#[derive(Deserialize)]
struct ImageAvailabilityRecord {
    #[serde(default)]
    caches: BTreeSet<LocalImageCacheKey>,
    #[serde(default, rename = "hosts")]
    _hosts: BTreeSet<String>,
    registry_ref: Option<String>,
    failure: Option<String>,
}

impl From<ImageAvailabilityRecord> for ImageAvailability {
    fn from(record: ImageAvailabilityRecord) -> Self {
        Self { caches: record.caches, registry_ref: record.registry_ref, failure: record.failure }
    }
}

/// A cache belongs to an exact provider instance on an exact host.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalImageCacheKey {
    pub host: String,
    pub provider_instance: String,
}

/// Stored host inventory. Legacy host-only lists decode without attributing
/// their contents to an arbitrary provider. Remove the list decoder one roll
/// after B1 (ADR 0047).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LocalImageInventories(pub BTreeMap<String, BTreeSet<String>>);

impl<'de> Deserialize<'de> for LocalImageInventories {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Record {
            PerCache(BTreeMap<String, BTreeSet<String>>),
            Legacy(BTreeSet<String>),
        }
        Ok(match Record::deserialize(deserializer)? {
            Record::PerCache(caches) => Self(caches),
            Record::Legacy(_digests) => Self::default(),
        })
    }
}

impl LocalImageInventories {
    /// An omitted instance resolves only when exactly one cache is observed.
    pub fn held(&self, instance: Option<&str>) -> Option<&BTreeSet<String>> {
        match instance {
            Some(instance) => self.0.get(instance),
            None if self.0.len() == 1 => self.0.values().next(),
            None => None,
        }
    }
}

pub const IMAGE_DIGESTS_CAPABILITY: &str = "image_digests";

pub fn is_image_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
}

/// Ordered acquisition cost for an exact digest. Availability is an observation,
/// never permission to substitute a different image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageAcquisitionCost {
    Held,
    RegistryPull,
    Build,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageInputStability;

    // #2972: a digest held by one provider is unavailable to another provider
    // on the same host; missing and ambiguous identities fail closed.
    #[hegel::test]
    fn cache_inventory_isolates_provider_instances(tc: hegel::TestCase) {
        // Covers empty inventories and duplicate digest observations in two caches.
        let duplicate = tc.draw(hegel::generators::booleans());
        let digest = format!("sha256:{:064x}", tc.draw(hegel::generators::integers::<u64>()));
        let mut inventory = LocalImageInventories::default();
        assert!(inventory.held(None).is_none());
        inventory.0.insert("a".into(), BTreeSet::from([digest.clone()]));
        assert!(inventory.held(Some("a")).expect("cache a").contains(&digest));
        assert!(inventory.held(Some("b")).is_none());
        assert_eq!(inventory.held(None), inventory.held(Some("a")));
        inventory.0.insert("b".into(), if duplicate { BTreeSet::from([digest.clone()]) } else { BTreeSet::new() });
        assert!(inventory.held(None).is_none());
        assert_eq!(inventory.held(Some("b")).expect("cache b").contains(&digest), duplicate);
        let encoded = serde_json::to_value(&inventory).expect("encode inventory");
        assert_eq!(serde_json::from_value::<LocalImageInventories>(encoded).expect("decode inventory"), inventory);
    }

    // ADR 0047: the previous generation's host-only records remain decodable;
    // they cannot be attributed to a specific provider, and new writes omit hosts.
    #[test]
    fn legacy_host_inventory_decodes_without_claiming_a_cache() {
        let inventory: LocalImageInventories = serde_json::from_str(r#"["sha256:old"]"#).expect("old inventory");
        assert!(inventory.0.is_empty());
        let availability: ImageAvailability =
            serde_json::from_str(r#"{"hosts":["host-a"],"registry_ref":null,"failure":null}"#).expect("old availability");
        assert!(availability.caches.is_empty());
        assert!(serde_json::to_value(availability).expect("new availability").get("hosts").is_none());
    }

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
