use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    image_identity,
    image_layer::{retired_dispatch, RetiredMarker, Shape},
    ApiPaths, ImageLayerSpec, ImageLayerTier, InputMeta, ReplicationClass, Resource, ResourceError, StatusPatch,
};

/// Infrastructure failures retry in place at most this many attempts in total.
pub const IMAGE_BUILD_MAX_ATTEMPTS: u32 = 5;

/// One build of one composed identity for one architecture (ADR 0062).
/// Infrastructure failures retry in place; recipe failures and successful
/// builds are terminal for that identity. Deleting the ImageBuild is the
/// operator retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBuild;
impl Resource for ImageBuild {
    type Spec = ImageBuildResourceSpec;
    type Status = ImageBuildStatus;
    type StatusPatch = ImageBuildStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "imagebuilds", kind: "ImageBuild" };
    const QUERYABLE_FIELDS: &'static [&'static str] = &["metadata.name", "spec.identity"];
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec(meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        let ImageBuildResourceSpec::Current(spec) = spec else { return Ok(()) };
        spec.validate().map_err(ResourceError::invalid)?;
        let expected = spec.resource_name();
        if meta.name != expected {
            return Err(ResourceError::invalid(format!("image build for this identity, architecture and delivery is named `{expected}`")));
        }
        Ok(())
    }

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current != requested {
            return Err(ResourceError::invalid("image build inputs are immutable"));
        }
        Ok(())
    }

    const STATUS_LIFECYCLE_KIND: Option<crate::lifecycle_reads::ProvisioningKind> =
        Some(crate::lifecycle_reads::ProvisioningKind::ImageBuild);

    fn validate_engine_status_update(_current: Option<&Self::Status>, _requested: &Self::Status) -> Result<(), ResourceError> {
        // The checked engine gates phase changes; this snapshot retains actuator observations.
        Ok(())
    }

    fn validate_status_update(current: Option<&Self::Status>, requested: &Self::Status) -> Result<(), ResourceError> {
        let Some(current) = current else { return Ok(()) };
        if current == requested {
            return Ok(());
        }
        match (current.phase, current.failure.as_ref().map(|failure| failure.class)) {
            (ImageBuildPhase::Built, _) => Err(ResourceError::invalid("a built image build is immutable")),
            (ImageBuildPhase::Failed, Some(ImageBuildFailureClass::Infrastructure))
                if requested.phase == ImageBuildPhase::Queued && current.attempt < IMAGE_BUILD_MAX_ATTEMPTS =>
            {
                Ok(())
            }
            (ImageBuildPhase::Failed, _) => {
                Err(ResourceError::invalid("a failed image build only returns to Queued after a bounded infrastructure failure"))
            }
            _ => Ok(()),
        }
    }
}

/// `image-<first 40 hex of identity>-<architecture>` for registry delivery.
/// A local build exists only on its builder, so its name also carries the
/// sanitised builder host: `image-<hex>-<architecture>-<host>`.
pub fn image_build_name(identity: &str, architecture: &str, delivery: &ImageDelivery, host_ref: &str) -> String {
    let hex = identity.strip_prefix("sha256:").unwrap_or(identity);
    let name = format!("image-{}-{architecture}", &hex[..hex.len().min(40)]);
    match delivery {
        ImageDelivery::Registry { .. } => name,
        ImageDelivery::Local => {
            let host = host_ref
                .chars()
                .map(|c| c.to_ascii_lowercase())
                .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' })
                .collect::<String>();
            format!("{name}-{}", host.trim_matches('-'))
        }
    }
}

/// The stored spec of an ImageBuild: a current per-identity build, or a
/// retired per-stage execution record that controllers ignore. Retired values
/// serialize as `{"retired": true}`.
///
/// ADR 0047: remove `Retired` one fleet roll after ADR 0062 builds land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageBuildResourceSpec {
    Current(Box<ImageBuildSpec>),
    Retired,
}

impl ImageBuildResourceSpec {
    pub fn current(&self) -> Option<&ImageBuildSpec> {
        match self {
            Self::Current(spec) => Some(spec),
            Self::Retired => None,
        }
    }

    pub fn into_current(self) -> Option<ImageBuildSpec> {
        match self {
            Self::Current(spec) => Some(*spec),
            Self::Retired => None,
        }
    }

    pub fn is_retired(&self) -> bool {
        matches!(self, Self::Retired)
    }
}

impl From<ImageBuildSpec> for ImageBuildResourceSpec {
    fn from(spec: ImageBuildSpec) -> Self {
        Self::Current(Box::new(spec))
    }
}

impl Serialize for ImageBuildResourceSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Current(spec) => spec.serialize(serializer),
            Self::Retired => RetiredMarker { retired: true }.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ImageBuildResourceSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match retired_dispatch(&value, "identity", "recipe_key") {
            Shape::Current => serde_json::from_value(value).map(|spec: ImageBuildSpec| spec.into()).map_err(serde::de::Error::custom),
            Shape::Retired => {
                serde_json::from_value::<RetiredImageBuildRecord>(value).map_err(serde::de::Error::custom)?;
                Ok(Self::Retired)
            }
            Shape::Marker => {
                serde_json::from_value::<RetiredMarker>(value).map_err(serde::de::Error::custom)?;
                Ok(Self::Retired)
            }
            Shape::Unknown => Err(serde::de::Error::custom("image build spec needs `identity` (ADR 0062 build)")),
        }
    }
}

/// ADR 0053 per-stage execution record. ADR 0047: remove one roll after
/// ADR 0062 builds land.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredImageBuildRecord {
    #[serde(rename = "recipe_key")]
    _recipe_key: serde::de::IgnoredAny,
    #[serde(default, rename = "execution_nonce")]
    _execution_nonce: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "inputs")]
    _inputs: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "layer")]
    _layer: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "host_ref")]
    _host_ref: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "reservation")]
    _reservation: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "parent_build_ref")]
    _parent_build_ref: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "previous_build_ref")]
    _previous_build_ref: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "attempt")]
    _attempt: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "not_before")]
    _not_before: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "reason")]
    _reason: Option<serde::de::IgnoredAny>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildSpec {
    /// [`image_identity`] of `layers`.
    pub identity: String,
    /// Canonical architecture: `amd64` or `arm64`.
    pub architecture: String,
    /// Ordered composition: base, shared, project, agent runtime.
    pub layers: Vec<ImageLayerSpec>,
    /// Builder host.
    pub host_ref: String,
    pub delivery: ImageDelivery,
    pub reservation: ImageBuildReservation,
}

impl ImageBuildSpec {
    /// The one name this build may have; see [`image_build_name`].
    pub fn resource_name(&self) -> String {
        image_build_name(&self.identity, &self.architecture, &self.delivery, &self.host_ref)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.architecture.as_str(), "amd64" | "arm64") {
            return Err(format!("image build architecture `{}` must be amd64 or arm64", self.architecture));
        }
        if self.host_ref.is_empty() || self.reservation.cpu == 0 || self.reservation.disk_bytes == 0 {
            return Err("image build needs a host and positive CPU and disk reservation".into());
        }
        if let ImageDelivery::Registry { repository } = &self.delivery {
            if repository.trim().is_empty() || repository.contains(['@', ' ']) {
                return Err("image build registry delivery needs a repository without tag or digest".into());
            }
        }
        if self.layers.first().is_none_or(|layer| layer.tier != ImageLayerTier::Base)
            || self.layers.iter().skip(1).any(|layer| layer.tier == ImageLayerTier::Base)
            || self.layers.windows(2).any(|pair| pair[0].tier > pair[1].tier)
        {
            return Err("image build layers start with one base layer and follow tier order".into());
        }
        let names = self.layers.iter().map(|layer| layer.name.as_str()).collect::<BTreeSet<_>>();
        if names.len() != self.layers.len() {
            return Err("image build layers have distinct names".into());
        }
        for layer in &self.layers {
            layer.validate()?;
        }
        if image_identity(&self.layers) != self.identity {
            return Err("image build identity does not match its layers".into());
        }
        Ok(())
    }
}

/// Where a built image goes. `Local` is single-host mode without a registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageDelivery {
    Registry { repository: String },
    Local,
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

/// Infrastructure failures retry with backoff; recipe failures never retry
/// identical inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageBuildFailureClass {
    // ADR 0047: remove the ADR 0053 aliases one roll after ADR 0062 builds land.
    #[serde(alias = "transient")]
    Infrastructure,
    #[serde(alias = "deterministic")]
    Recipe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildFailure {
    pub class: ImageBuildFailureClass,
    pub reason: String,
}

/// One layer's probe outcome on the composed image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageLayerProbe {
    pub layer: String,
    pub outcome: ImageProbeOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageProbeOutcome {
    Passed,
    Failed { reason: String },
    NotRun,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ImageBuildStatusRecord")]
pub struct ImageBuildStatus {
    pub phase: ImageBuildPhase,
    /// Attempts started, including the current one.
    #[serde(default)]
    pub attempt: u32,
    /// Earliest time a queued retry may start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Registry delivery: `<repository>@sha256:…`; local delivery: the `sha256:…` image ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ImageLayerProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ImageBuildFailure>,
}

impl ImageBuildStatus {
    /// Whether this status returns to Queued: a bounded infrastructure
    /// failure with attempts remaining. Recipe failures, failures without a
    /// recorded class and exhausted attempts are terminal.
    pub fn will_retry(&self) -> bool {
        self.phase == ImageBuildPhase::Failed
            && self.failure.as_ref().is_some_and(|failure| failure.class == ImageBuildFailureClass::Infrastructure)
            && self.attempt < IMAGE_BUILD_MAX_ATTEMPTS
    }

    /// Whether the build has reached its final outcome for this identity:
    /// built, or failed with no retry to come.
    pub fn is_settled(&self) -> bool {
        match self.phase {
            ImageBuildPhase::Built => true,
            ImageBuildPhase::Failed => !self.will_retry(),
            ImageBuildPhase::Queued | ImageBuildPhase::Building => false,
        }
    }
}

/// Accepted stored status. ADR 0053 fields (`availability`, `parent_digest`,
/// `identity`, `verified_provides`) are accepted and dropped. ADR 0047: remove
/// them one roll after ADR 0062 builds land.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageBuildStatusRecord {
    #[serde(default)]
    phase: ImageBuildPhase,
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    not_before: Option<DateTime<Utc>>,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    probes: Vec<ImageLayerProbe>,
    #[serde(default)]
    log_ref: Option<String>,
    #[serde(default)]
    failure: Option<ImageBuildFailure>,
    #[serde(default, rename = "availability")]
    _availability: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "parent_digest")]
    _parent_digest: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "identity")]
    _identity: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "verified_provides")]
    _verified_provides: Option<serde::de::IgnoredAny>,
}

impl From<ImageBuildStatusRecord> for ImageBuildStatus {
    fn from(record: ImageBuildStatusRecord) -> Self {
        Self {
            phase: record.phase,
            attempt: record.attempt,
            not_before: record.not_before,
            started_at: record.started_at,
            finished_at: record.finished_at,
            digest: record.digest,
            probes: record.probes,
            log_ref: record.log_ref,
            failure: record.failure,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageBuildStatusPatch {
    /// Begin attempt `attempt`, clearing the previous attempt's outcome.
    Start {
        at: DateTime<Utc>,
        attempt: u32,
    },
    Built {
        at: DateTime<Utc>,
        digest: String,
        probes: Vec<ImageLayerProbe>,
        log_ref: String,
    },
    Failed {
        at: DateTime<Utc>,
        failure: ImageBuildFailure,
        probes: Vec<ImageLayerProbe>,
        log_ref: String,
    },
    /// Requeue after an infrastructure failure; the failure stays as the reason.
    Retry {
        not_before: DateTime<Utc>,
    },
}
impl StatusPatch<ImageBuildStatus> for ImageBuildStatusPatch {
    fn apply(&self, status: &mut ImageBuildStatus) {
        match self {
            Self::Start { at, attempt } => {
                status.phase = ImageBuildPhase::Building;
                status.attempt = *attempt;
                status.not_before = None;
                status.started_at = Some(*at);
                status.finished_at = None;
                status.digest = None;
                status.probes.clear();
                status.failure = None;
            }
            Self::Built { at, digest, probes, log_ref } => {
                status.phase = ImageBuildPhase::Built;
                status.finished_at = Some(*at);
                status.digest = Some(digest.clone());
                status.probes = probes.clone();
                status.log_ref = Some(log_ref.clone());
                status.failure = None;
            }
            Self::Failed { at, failure, probes, log_ref } => {
                status.phase = ImageBuildPhase::Failed;
                status.finished_at = Some(*at);
                status.probes = probes.clone();
                status.failure = Some(failure.clone());
                status.log_ref = Some(log_ref.clone());
            }
            Self::Retry { not_before } => {
                status.phase = ImageBuildPhase::Queued;
                status.not_before = Some(*not_before);
            }
        }
    }
}

/// A placed image's local identity and, when pulled from a registry, its
/// repository digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacedImageIdentity {
    pub local_image_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_digest: Option<String>,
}

impl PlacedImageIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if self.local_image_id.trim().is_empty() || self.registry_digest.as_ref().is_some_and(|digest| digest.trim().is_empty()) {
            return Err("placed image identity is empty".into());
        }
        Ok(())
    }
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
mod tests;
