use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use flotilla_protocol::ConfiguredResourceLimits;
use serde::{Deserialize, Serialize};

use crate::{
    placement_policy::DockerImagePullPolicy, resource::define_resource, status_patch::StatusPatch, ControllerRetry, ResourceError,
};

define_resource!(
    Environment,
    "environments",
    EnvironmentSpec,
    EnvironmentStatus,
    EnvironmentStatusPatch,
    replication = crate::ReplicationClass::None,
    canonicalize_spec = canonicalize_environment_spec,
    queryable = &[
        "metadata.name",
        "spec.provider_address",
        "spec.host_direct",
        "spec.host_direct.host_ref",
        "status.runtime_observation.container_id"
    ]
);

/// Shared namespace for host-direct Environments and seeded PlacementPolicies.
/// Name construction and placement field ownership must use the same prefix.
pub const HOST_DIRECT_NAME_PREFIX: &str = "host-direct-";

/// Resource reference for a host's direct execution environment.
pub fn host_direct_environment_name(host_ref: &str) -> String {
    format!("{HOST_DIRECT_NAME_PREFIX}{host_ref}")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
pub struct EnvironmentSpec {
    pub provider_address: Option<flotilla_address::Address>,
    pub host_direct: Option<HostDirectEnvironmentSpec>,
    pub docker: Option<DockerEnvironmentSpec>,
    pub cloud_sandbox: Option<CloudSandboxEnvironmentSpec>,
}

// A preceding-generation Environment can bind a non-default instance in its
// metadata. Resolve it while that context is present, before Serialize could
// select a built-in default. Explicit addresses always remain authoritative.
fn canonicalize_environment_spec<'a>(
    labels: &BTreeMap<String, String>,
    mut spec: Cow<'a, EnvironmentSpec>,
) -> Result<Cow<'a, EnvironmentSpec>, ResourceError> {
    if spec.provider_address.is_none() {
        if let Some(instance) = labels.get("flotilla.work/environment-provider-instance") {
            let address = spec.resolved_provider_address(Some(instance)).map_err(ResourceError::invalid)?;
            spec.to_mut().provider_address = address;
        }
    }
    Ok(spec)
}

/// Only host/account places followed by one provider instance name are valid.
pub fn validate_environment_provider_address(address: &flotilla_address::Address) -> Result<(), String> {
    use flotilla_address::Kind;
    let segments = address.segments();
    if segments.len() == 2 && matches!(segments[0].kind(), Kind::Host | Kind::Account) && segments[1].kind() == Kind::Provider {
        Ok(())
    } else {
        Err(format!("expected host:<host>/provider:<instance> or account:<account>/provider:<instance>, got {address}"))
    }
}

impl EnvironmentSpec {
    pub fn host_ref(&self) -> Option<&str> {
        if let Some(address) = &self.provider_address {
            return address.get(flotilla_address::Kind::Host);
        }
        self.host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| self.docker.as_ref().map(|spec| spec.host_ref.as_str()))
            .filter(|host| !host.is_empty())
    }

    pub fn resolved_provider_address(&self, instance: Option<&str>) -> Result<Option<flotilla_address::Address>, String> {
        if let Some(address) = &self.provider_address {
            validate_environment_provider_address(address)?;
            return Ok(Some(address.clone()));
        }
        let Some(host) = self.host_ref() else { return Ok(None) };
        let default_instance = if self.host_direct.is_some() { "host-direct" } else { "docker" };
        flotilla_address::Address::root(flotilla_address::Kind::Host, host)
            .and_then(|place| place.child(flotilla_address::Kind::Provider, instance.unwrap_or(default_instance)))
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

// Canonical writes are centralized here so every resource author, including
// manifest ingestion, writes the new address and never legacy host fields.
// Resource::canonicalize_spec first resolves legacy instance labels while the
// metadata context is available.
impl Serialize for EnvironmentSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Stored<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            provider_address: Option<flotilla_address::Address>,
            #[serde(skip_serializing_if = "Option::is_none")]
            host_direct: &'a Option<HostDirectEnvironmentSpec>,
            #[serde(skip_serializing_if = "Option::is_none")]
            docker: &'a Option<DockerEnvironmentSpec>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cloud_sandbox: &'a Option<CloudSandboxEnvironmentSpec>,
        }
        let provider_address = self.resolved_provider_address(None).map_err(serde::ser::Error::custom)?;
        Stored { provider_address, host_direct: &self.host_direct, docker: &self.docker, cloud_sandbox: &self.cloud_sandbox }
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EnvironmentSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(default)]
            provider_address: Option<flotilla_address::Address>,
            #[serde(default)]
            host_direct: Option<HostDirectEnvironmentSpec>,
            #[serde(default)]
            docker: Option<DockerEnvironmentSpec>,
            #[serde(default)]
            cloud_sandbox: Option<CloudSandboxEnvironmentSpec>,
        }
        let mut stored = Stored::deserialize(deserializer)?;
        if let Some(address) = &stored.provider_address {
            validate_environment_provider_address(address).map_err(serde::de::Error::custom)?;
            // Keep transitional in-memory consumers consistent with the explicit
            // address. These compatibility fields are never serialized.
            let host = address.get(flotilla_address::Kind::Host).unwrap_or_default();
            if let Some(spec) = &mut stored.host_direct {
                spec.host_ref = host.into();
            }
            if let Some(spec) = &mut stored.docker {
                spec.host_ref = host.into();
            }
        }
        Ok(Self {
            provider_address: stored.provider_address,
            host_direct: stored.host_direct,
            docker: stored.docker,
            cloud_sandbox: stored.cloud_sandbox,
        })
    }
}

/// Additive cloud input approved on #3620 (2026-10-22). No credentials or reach recipes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CloudSandboxEnvironmentSpec {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub probe_needs: BTreeSet<crate::CapabilityNeed>,
    pub image: String,
    pub size: CloudSandboxSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_cap_seconds: Option<u64>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for CloudSandboxEnvironmentSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            image: String,
            size: CloudSandboxSize,
            lifetime_cap_seconds: Option<u64>,
            #[serde(default)]
            env: BTreeMap<String, String>,
            probe_needs: Option<BTreeSet<crate::CapabilityNeed>>,
        }
        let mut stored = Stored::deserialize(deserializer)?;
        // ADR 0047: remove one fleet roll after #3765. Lift preceding-generation
        // controls on decode; write only typed fields. Explicit new fields win.
        let needs = stored.env.remove("FLOTILLA_CLOUD_IMAGE_NEEDS");
        stored.env.remove("FLOTILLA_DRYDOCK_LAB_FORWARD");
        let probe_needs = match stored.probe_needs {
            Some(needs) => needs,
            None => needs.map(|value| serde_json::from_str(&value)).transpose().map_err(serde::de::Error::custom)?.unwrap_or_default(),
        };
        Ok(Self { image: stored.image, size: stored.size, lifetime_cap_seconds: stored.lifetime_cap_seconds, env: stored.env, probe_needs })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSandboxSize {
    pub cpus: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

/// Immutable provider facts recorded when a cloud backing is provisioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentProviderDescriptor {
    pub instance: String,
    pub kind: flotilla_protocol::provider::ProviderKind,
    pub image_digest: String,
    pub host_version: String,
    pub module_version: String,
    pub capabilities: flotilla_protocol::provider::ProviderCapabilities,
    /// What the provider reports it chose to reach the vessel's agent. Shown,
    /// never decided on. Absent when it reports none, and in records written
    /// before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_connection: Option<flotilla_protocol::provider::AgentConnectionReport>,
}

/// Durable references only; recovery owns their retention and deletion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSurvivors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectEnvironmentSpec {
    /// ADR 0047: decode legacy host placement for one roll after #3521.
    #[serde(default, skip_serializing)]
    pub host_ref: String,
    pub repo_default_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// The retired ADR 0053 `image_composition` field is accepted and dropped by
// this type's lenient decoding; ADR 0047: no shim to remove, but keep this
// type lenient until one roll after ADR 0062 images land.
pub struct DockerEnvironmentSpec {
    /// ADR 0047: decode legacy host placement for one roll after #3521.
    #[serde(default, skip_serializing)]
    pub host_ref: String,
    pub image: String,
    /// The per-identity ImageBuild that realises `image`.
    /// ADR 0047: remove decoder default one roll after build references land.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_build_ref: Option<String>,
    /// Agent adapters the placement policy expects discovery to find in the image.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub declared_agent_adapters: BTreeSet<String>,
    /// Agent adapters this specific vessel workflow will actually launch.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required_agent_adapters: BTreeSet<String>,
    #[serde(default)]
    pub pull_policy: DockerImagePullPolicy,
    /// ADR 0047: default old records for one fleet roll.
    #[serde(default)]
    pub memory_policy: EnvironmentMemoryPolicy,
    #[serde(default)]
    pub mounts: Vec<EnvironmentMount>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentMount {
    pub source_path: String,
    pub target_path: String,
    pub mode: EnvironmentMountMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentMountMode {
    Ro,
    Rw,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentPhase {
    #[default]
    Pending,
    Provisioning,
    Ready,
    // Retired unwritten Terminating decodes as Lost, the nearest unavailable
    // backing state, not a semantic rename. ADR 0047: remove one roll after #2917.
    #[serde(alias = "Terminating")]
    Lost,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
// The retired ADR 0053 `image_build_refs` list is accepted and dropped by
// this type's lenient decoding (ADR 0047); `DockerEnvironmentSpec.image_build_ref`
// names the single per-identity build.
pub struct EnvironmentStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_descriptor: Option<EnvironmentProviderDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survivors: Option<EnvironmentSurvivors>,
    /// Configured limits, not usage. Remove the decoder default one fleet roll
    /// after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub phase: EnvironmentPhase,
    /// ADR 0047: absent in old records; keep default for one fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_observation: Option<flotilla_protocol::EnvironmentRuntimeObservation>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // ADR 0047: remove image_digest alias one roll after generation 1.
    #[serde(alias = "image_digest")]
    pub local_image_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_delivery_retry: Option<ControllerRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_refresh_retry: Option<ControllerRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentStatusPatch {
    MarkCloudReady {
        backing_id: String,
        image_ref: String,
        provider_descriptor: Box<EnvironmentProviderDescriptor>,
        survivors: EnvironmentSurvivors,
    },
    ObserveRuntime {
        observation: flotilla_protocol::EnvironmentRuntimeObservation,
    },
    MarkReady {
        configured_limits: Option<ConfiguredResourceLimits>,
        docker_container_id: Option<String>,
        image_ref: Option<String>,
        local_image_id: Option<String>,
        registry_digest: Option<String>,
    },
    WaitForImageBuild {
        message: String,
    },
    MarkLost {
        message: String,
    },
    MarkFailed {
        message: String,
    },
    CredentialDelivery {
        retry: Option<ControllerRetry>,
    },
    CredentialRefresh {
        retry: Option<ControllerRetry>,
    },
}

impl StatusPatch<EnvironmentStatus> for EnvironmentStatusPatch {
    fn apply(&self, status: &mut EnvironmentStatus) {
        match self {
            Self::MarkCloudReady { backing_id, image_ref, provider_descriptor, survivors } => {
                status.phase = EnvironmentPhase::Ready;
                status.ready = true;
                status.backing_id = Some(backing_id.clone());
                status.image_ref = Some(image_ref.clone());
                status.local_image_id = image_ref.rsplit_once('@').map(|(_, digest)| digest.to_owned());
                status.registry_digest = Some(image_ref.clone());
                status.provider_descriptor = Some((**provider_descriptor).clone());
                status.survivors = Some(survivors.clone());
                status.message = None;
            }
            Self::ObserveRuntime { observation } => {
                if let Some(inspected) = &observation.configured_limits {
                    let limits = status.configured_limits.get_or_insert_with(Default::default);
                    // Inspection is authoritative, including removal of an old quota.
                    limits.cpus = inspected.cpus;
                    limits.cpu_shares = inspected.cpu_shares;
                }
                status.runtime_observation.get_or_insert_with(Default::default).merge(observation);
            }
            Self::CredentialDelivery { retry } => status.credential_delivery_retry = retry.clone(),
            Self::CredentialRefresh { retry } => status.credential_refresh_retry = retry.clone(),
            Self::MarkReady { configured_limits, docker_container_id, image_ref, local_image_id, registry_digest } => {
                status.configured_limits = configured_limits.clone();
                status.phase = EnvironmentPhase::Ready;
                status.ready = true;
                status.docker_container_id = docker_container_id.clone();
                status.image_ref = image_ref.clone();
                status.local_image_id = local_image_id.clone();
                status.registry_digest = registry_digest.clone();
                status.message = None;
            }
            Self::WaitForImageBuild { message } => {
                status.phase = EnvironmentPhase::Provisioning;
                status.ready = false;
                status.message = Some(message.clone());
            }
            Self::MarkLost { message } => {
                status.phase = EnvironmentPhase::Lost;
                status.ready = false;
                status.message = Some(message.clone());
            }
            Self::MarkFailed { message } => {
                status.phase = EnvironmentPhase::Failed;
                status.ready = false;
                status.message = Some(message.clone());
            }
        }
    }
}

/// Placement-authored budget. 80% of host RAM is divided by four for each
/// crew's cap (about 12 GiB on a 61 GiB host). Caps are limits, not
/// reservations: a 7.7 GiB cap OOM-killed workspace test builds (#2751).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentMemoryPolicy {
    pub host_memory_percent: u8,
    pub expected_concurrent_crews: u32,
    /// Swap per crew, in bytes. Zero prevents zram/host swap pressure.
    pub swap_bytes: u64,
}

impl Default for EnvironmentMemoryPolicy {
    fn default() -> Self {
        Self { host_memory_percent: 80, expected_concurrent_crews: 4, swap_bytes: 0 }
    }
}

impl EnvironmentMemoryPolicy {
    pub fn resolve(&self, host_memory_bytes: u64) -> Result<flotilla_protocol::EnvironmentMemoryLimits, String> {
        if !(1..=100).contains(&self.host_memory_percent) || self.expected_concurrent_crews == 0 {
            return Err("memory policy requires host_memory_percent in 1..=100 and expected_concurrent_crews > 0".into());
        }
        let memory_bytes = (u128::from(host_memory_bytes) * u128::from(self.host_memory_percent)
            / 100
            / u128::from(self.expected_concurrent_crews)) as u64;
        if memory_bytes < 6 * 1024 * 1024 {
            return Err("host memory budget is below Docker's 6 MiB minimum; refusing an unlimited container".into());
        }
        memory_bytes.checked_add(self.swap_bytes).ok_or("memory and swap limit overflow")?;
        Ok(flotilla_protocol::EnvironmentMemoryLimits { memory_bytes, swap_bytes: self.swap_bytes })
    }
}

#[cfg(test)]
mod retired_image_field_tests {
    use super::*;

    // ADR 0047: ADR 0053 Environments carried a frozen composition and a
    // staged build list; both decode and are dropped on the next write.
    #[test]
    fn retired_composition_and_build_refs_are_dropped() {
        let spec: EnvironmentSpec = serde_json::from_value(serde_json::json!({
            "docker": {
                "host_ref": "feta", "image": "sha256:abc", "image_build_ref": "image-abc-amd64",
                "image_composition": {"selection": {"base": "base", "utilities": null, "harness": null, "project": null}, "layers": []}
            }
        }))
        .expect("old environment spec decodes");
        let encoded = serde_json::to_value(&spec).expect("encode");
        assert!(encoded["docker"].get("image_composition").is_none());
        assert_eq!(encoded["docker"]["image_build_ref"], "image-abc-amd64");
        let status: EnvironmentStatus =
            serde_json::from_value(serde_json::json!({"phase": "Provisioning", "image_build_refs": ["a", "b"]})).expect("old status");
        assert_eq!(status.phase, EnvironmentPhase::Provisioning);
        assert!(serde_json::to_value(&status).expect("encode").get("image_build_refs").is_none());
    }
}

#[cfg(test)]
mod memory_tests {
    use hegel::generators as gs;

    use super::*;

    // A per-crew quota never exceeds the configured host budget; swap is
    // explicit. Generate zero, the Docker minimum, large hosts and overflow.
    #[hegel::test]
    fn memory_policy_conserves_host_budget(tc: hegel::TestCase) {
        let host = tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX));
        let crews = tc.draw(gs::integers::<u32>().min_value(1).max_value(128));
        let percent = tc.draw(gs::integers::<u8>().min_value(1).max_value(100));
        let swap = tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX));
        let policy = EnvironmentMemoryPolicy { host_memory_percent: percent, expected_concurrent_crews: crews, swap_bytes: swap };
        let budget = u128::from(host) * u128::from(percent) / 100;
        let quota = budget / u128::from(crews);
        match policy.resolve(host) {
            Ok(limits) => {
                assert_eq!(u128::from(limits.memory_bytes), quota);
                assert!(u128::from(limits.memory_bytes) * u128::from(crews) <= budget);
                assert!(limits.memory_bytes >= 6 * 1024 * 1024);
                assert_eq!(limits.swap_bytes, swap);
                assert!(limits.memory_bytes.checked_add(swap).is_some());
            }
            Err(_) => assert!(quota < 6 * 1024 * 1024 || quota + u128::from(swap) > u128::from(u64::MAX)),
        }
    }

    // Invalid budgets refuse provisioning rather than creating unlimited crews.
    #[test]
    fn invalid_memory_policy_and_docker_minimum_are_rejected() {
        for (percent, crews) in [(0, 4), (101, 4), (50, 0)] {
            assert!(EnvironmentMemoryPolicy { host_memory_percent: percent, expected_concurrent_crews: crews, swap_bytes: 0 }
                .resolve(u64::MAX)
                .is_err());
        }
        let policy = EnvironmentMemoryPolicy { host_memory_percent: 100, expected_concurrent_crews: 1, swap_bytes: 0 };
        assert!(policy.resolve(6 * 1024 * 1024 - 1).is_err());
        assert_eq!(policy.resolve(6 * 1024 * 1024).expect("minimum").memory_bytes, 6 * 1024 * 1024);
    }
}

#[cfg(test)]
mod observation_tests {
    use flotilla_protocol::{EnvironmentExitCause, EnvironmentRuntimeObservation, EnvironmentTermination};
    use hegel::generators as gs;

    use super::*;

    // Adoption replaces CPU configuration facts but preserves launch parallelism.
    #[test]
    fn inspected_cpu_settings_replace_quota_without_erasing_build_jobs() {
        let mut status = EnvironmentStatus {
            configured_limits: Some(flotilla_protocol::ConfiguredResourceLimits {
                cpus: Some(8),
                build_jobs: Some(32),
                linker_threads: Some(32),
                ..Default::default()
            }),
            ..Default::default()
        };
        for (cpus, shares) in [(Some(8), None), (None, Some(1024)), (None, None)] {
            EnvironmentStatusPatch::ObserveRuntime {
                observation: EnvironmentRuntimeObservation {
                    configured_limits: Some(flotilla_protocol::ConfiguredResourceLimits { cpus, cpu_shares: shares, ..Default::default() }),
                    ..Default::default()
                },
            }
            .apply(&mut status);
            let limits = status.configured_limits.as_ref().expect("configured settings");
            assert_eq!((limits.cpus, limits.cpu_shares, limits.build_jobs, limits.linker_threads), (cpus, shares, Some(32), Some(32)));
        }
    }

    // Runtime observations preserve the last successful sample when later reads
    // are unavailable or backing has died. Generate empty/sample/death histories
    // with repeated observations; assert the invariant after every step.
    #[hegel::test]
    fn observations_retain_evidence_when_backing_disappears(tc: hegel::TestCase) {
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
        let mut status = EnvironmentStatus::default();
        let mut last = None;
        let mut terminated = false;
        for _ in 0..steps {
            let operation = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let observation = match operation {
                0 => EnvironmentRuntimeObservation::default(),
                1 => {
                    let bytes = tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX));
                    last = Some(bytes);
                    EnvironmentRuntimeObservation {
                        memory_usage_bytes: Some(bytes),
                        memory_observed_at: Some("sample time".into()),
                        ..Default::default()
                    }
                }
                _ => {
                    terminated = true;
                    EnvironmentRuntimeObservation {
                        termination: Some(
                            EnvironmentTermination::builder()
                                .exit_code(137)
                                .signal(9)
                                .oom_killed(true)
                                .cause(EnvironmentExitCause::CgroupOom)
                                .finished_at("finish time".into())
                                .build(),
                        ),
                        ..Default::default()
                    }
                }
            };
            let patch = EnvironmentStatusPatch::ObserveRuntime { observation };
            patch.apply(&mut status);
            let after = status.clone();
            patch.apply(&mut status);
            assert_eq!(status, after, "duplicate observations are idempotent");
            let evidence = status.runtime_observation.as_ref().expect("observation");
            assert_eq!(evidence.memory_usage_bytes, last);
            assert_eq!(evidence.termination.is_some(), terminated);
            if last.is_some() {
                assert_eq!(evidence.memory_observed_at.as_deref(), Some("sample time"));
            }
        }
    }
}

#[cfg(test)]
mod provider_address_tests {
    use super::*;
    use flotilla_address::{Address, Kind};
    use hegel::generators as gs;

    // ADR 0058: the explicit address owns provider selection even if legacy
    // host fields disagree. New writes contain only that address and inputs.
    #[hegel::test]
    fn explicit_provider_address_owns_placement(tc: hegel::TestCase) {
        // Both place classes, arbitrary escaped/case-sensitive identifiers,
        // both legacy input shapes, and conflicting legacy host identities.
        let place = tc.draw(gs::text().alphabet("aZ09 /:%é").min_size(1).max_size(24));
        let instance = tc.draw(gs::text().alphabet("aZ09 /:%é").min_size(1).max_size(24));
        let account = tc.draw(gs::booleans());
        let direct = tc.draw(gs::booleans());
        let address = Address::root(if account { Kind::Account } else { Kind::Host }, &place)
            .and_then(|place| place.child(Kind::Provider, &instance))
            .expect("qualified address");
        let mut value = serde_json::json!({"provider_address": address.to_string()});
        value[if direct { "host_direct" } else { "docker" }] = if direct {
            serde_json::json!({"host_ref":"obsolete-host", "repo_default_dir":"/workspace"})
        } else {
            serde_json::json!({"host_ref":"obsolete-host", "image":"image"})
        };
        let spec: EnvironmentSpec = serde_json::from_value(value).expect("decode explicit address");
        assert_eq!(spec.resolved_provider_address(Some("obsolete-instance")).expect("resolve"), Some(address.clone()));
        assert_eq!(spec.host_ref(), (!account).then_some(place.as_str()));
        let encoded = serde_json::to_value(&spec).expect("encode");
        assert_eq!(encoded["provider_address"], address.to_string());
        assert!(encoded[if direct { "host_direct" } else { "docker" }].get("host_ref").is_none());
        let decoded: EnvironmentSpec = serde_json::from_value(encoded).expect("read new shape");
        assert_eq!(decoded, spec);
    }

    // ADR 0047: preceding-generation host fields still decode, and the next
    // write retires them in favour of an address without losing the place.
    #[hegel::test]
    fn legacy_host_inputs_write_only_provider_address(tc: hegel::TestCase) {
        let host = tc.draw(gs::text().alphabet("aZ09 /:%é").min_size(1).max_size(24));
        let direct = tc.draw(gs::booleans());
        let value = if direct {
            serde_json::json!({"host_direct":{"host_ref":host,"repo_default_dir":"/work"}})
        } else {
            serde_json::json!({"docker":{"host_ref":host,"image":"image"}})
        };
        let spec: EnvironmentSpec = serde_json::from_value(value).expect("legacy input");
        let encoded = serde_json::to_value(&spec).expect("canonical write");
        let address: Address = serde_json::from_value(encoded["provider_address"].clone()).expect("address written");
        assert_eq!(address.get(Kind::Host), Some(host.as_str()));
        assert_eq!(address.name(), if direct { "host-direct" } else { "docker" });
        assert!(encoded[if direct { "host_direct" } else { "docker" }].get("host_ref").is_none());
    }

    // Invalid explicit addresses must never be replaced by the legacy host.
    #[test]
    fn invalid_explicit_provider_address_fails_closed() {
        for address in ["", "host:h", "host:h/provider:", "project:p", "usage:p/account:a/provider:i", "host:h/provider:i/provider:j"] {
            assert!(
                serde_json::from_value::<EnvironmentSpec>(serde_json::json!({
                    "provider_address": address, "host_direct":{"host_ref":"fallback", "repo_default_dir":"/work"}
                }))
                .is_err(),
                "invalid explicit address {address:?}"
            );
        }
    }
}

#[cfg(test)]
mod cloud_schema_tests {
    use super::*;
    use hegel::generators as gs;
    // #3620's approved additive schema round-trips arbitrary positive sizes,
    // optional caps and environment values without inventing legacy Docker data.
    #[hegel::test]
    fn cloud_input_roundtrips_without_legacy_fields(tc: hegel::TestCase) {
        let cpus = tc.draw(gs::integers::<u32>().min_value(1));
        let memory_bytes = tc.draw(gs::integers::<u64>().min_value(1));
        let disk_bytes = tc.draw(gs::integers::<u64>().min_value(1));
        let lifetime_cap_seconds = tc.draw(gs::booleans()).then(|| tc.draw(gs::integers::<u64>().min_value(1)));
        let value = tc.draw(gs::text().max_size(30));
        let spec = EnvironmentSpec {
            provider_address: Some("account:test/provider:daytona".parse().unwrap()),
            cloud_sandbox: Some(CloudSandboxEnvironmentSpec {
                probe_needs: Default::default(),
                image: "registry/image@sha256:digest".into(),
                size: CloudSandboxSize { cpus, memory_bytes, disk_bytes },
                lifetime_cap_seconds,
                env: BTreeMap::from([("VALUE".into(), value)]),
            }),
            ..Default::default()
        };
        let encoded = serde_json::to_value(&spec).unwrap();
        assert!(encoded.get("docker").is_none());
        assert!(encoded.get("host_direct").is_none());
        assert_eq!(serde_json::from_value::<EnvironmentSpec>(encoded).unwrap(), spec);
        let old: EnvironmentSpec = serde_json::from_value(serde_json::json!({"docker":{"image":"previous-image"}})).unwrap();
        assert!(old.cloud_sandbox.is_none());
        let old_status: EnvironmentStatus =
            serde_json::from_value(serde_json::json!({"phase":"Ready","docker_container_id":"previous-backing"})).unwrap();
        assert_eq!(old_status.docker_container_id.as_deref(), Some("previous-backing"));
        assert!(old_status.backing_id.is_none() && old_status.provider_descriptor.is_none() && old_status.survivors.is_none());
    }
}
