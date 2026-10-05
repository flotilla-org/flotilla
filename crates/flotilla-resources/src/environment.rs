use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::ConfiguredResourceLimits;
use serde::{Deserialize, Serialize};

use crate::{placement_policy::DockerImagePullPolicy, resource::define_resource, status_patch::StatusPatch, ControllerRetry};

define_resource!(Environment, "environments", EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch);

/// Resource reference for a host's direct execution environment.
pub fn host_direct_environment_name(host_ref: &str) -> String {
    format!("host-direct-{host_ref}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_direct: Option<HostDirectEnvironmentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerEnvironmentSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectEnvironmentSpec {
    pub host_ref: String,
    pub repo_default_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerEnvironmentSpec {
    pub host_ref: String,
    pub image: String,
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
    Ready,
    Terminating,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentStatus {
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
    pub image_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_delivery_retry: Option<ControllerRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_refresh_retry: Option<ControllerRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentStatusPatch {
    ObserveRuntime {
        observation: flotilla_protocol::EnvironmentRuntimeObservation,
    },
    MarkReady {
        configured_limits: Option<ConfiguredResourceLimits>,
        docker_container_id: Option<String>,
        image_ref: Option<String>,
        image_digest: Option<String>,
    },
    MarkFailed {
        message: String,
    },
    MarkTerminating,
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
            Self::ObserveRuntime { observation } => status.runtime_observation.get_or_insert_with(Default::default).merge(observation),
            Self::CredentialDelivery { retry } => status.credential_delivery_retry = retry.clone(),
            Self::CredentialRefresh { retry } => status.credential_refresh_retry = retry.clone(),
            Self::MarkReady { configured_limits, docker_container_id, image_ref, image_digest } => {
                status.configured_limits = configured_limits.clone();
                status.phase = EnvironmentPhase::Ready;
                status.ready = true;
                status.docker_container_id = docker_container_id.clone();
                status.image_ref = image_ref.clone();
                status.image_digest = image_digest.clone();
                status.message = None;
            }
            Self::MarkFailed { message } => {
                status.phase = EnvironmentPhase::Failed;
                status.ready = false;
                status.message = Some(message.clone());
            }
            Self::MarkTerminating => {
                status.phase = EnvironmentPhase::Terminating;
                status.ready = false;
            }
        }
    }
}

/// Placement-authored budget. Half of host RAM is shared by four crews by
/// default, reserving the rest for host services and interactive workloads.
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
        Self { host_memory_percent: 50, expected_concurrent_crews: 4, swap_bytes: 0 }
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
