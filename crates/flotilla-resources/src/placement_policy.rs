use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch};

define_resource!(PlacementPolicy, "placementpolicies", PlacementPolicySpec, (), NoStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementPolicySpec {
    pub pool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_direct: Option<HostDirectPlacementPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_per_vessel: Option<DockerPerVesselPlacementPolicySpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectPlacementPolicySpec {
    pub host_ref: String,
    pub checkout: HostDirectPlacementPolicyCheckout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostDirectPlacementPolicyCheckout {
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerPerVesselPlacementPolicySpec {
    pub host_ref: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub checkout: DockerCheckoutStrategy,
    /// Agent adapter ids bundled in `image`, advertised so convoy admission can validate
    /// a crew's required capability against the image before provisioning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_adapters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerCheckoutStrategy {
    WorktreeOnHostAndMount { mount_path: String },
    FreshCloneInContainer { clone_path: String },
}

/// Validates authoring-time invariants for a placement policy spec. This does not check
/// admission-time concerns (e.g. whether a convoy's crew capabilities are satisfied by the
/// advertised `agent_adapters`) — only that the spec itself is well-formed.
pub fn validate_placement_policy(spec: &PlacementPolicySpec) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    if let Some(docker) = &spec.docker_per_vessel {
        let mut seen = BTreeSet::new();
        for adapter in &docker.agent_adapters {
            if adapter.trim().is_empty() {
                errors.push("agent_adapters entries must not be blank".to_string());
            } else if !seen.insert(adapter.as_str()) {
                errors.push(format!("duplicate agent adapter `{adapter}` in agent_adapters"));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_spec(agent_adapters: Vec<String>) -> PlacementPolicySpec {
        PlacementPolicySpec::builder()
            .pool("docker".to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                host_ref: "01HXYZ".to_string(),
                image: "ghcr.io/flotilla/dev:latest".to_string(),
                default_cwd: None,
                env: BTreeMap::new(),
                checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                agent_adapters,
            })
            .build()
    }

    #[test]
    fn validate_accepts_empty_and_distinct_adapters() {
        assert_eq!(validate_placement_policy(&docker_spec(vec![])), Ok(()));
        assert_eq!(validate_placement_policy(&docker_spec(vec!["codex".to_string(), "claude-code".to_string()])), Ok(()));
    }

    #[test]
    fn validate_rejects_blank_adapter_entries() {
        let err = validate_placement_policy(&docker_spec(vec!["".to_string()])).expect_err("blank adapter should fail");
        assert_eq!(err, vec!["agent_adapters entries must not be blank".to_string()]);
    }

    #[test]
    fn validate_rejects_duplicate_adapter_entries() {
        let err =
            validate_placement_policy(&docker_spec(vec!["codex".to_string(), "codex".to_string()])).expect_err("duplicate adapter should fail");
        assert_eq!(err, vec!["duplicate agent adapter `codex` in agent_adapters".to_string()]);
    }

    #[test]
    fn agent_adapters_round_trips_through_yaml() {
        let spec = docker_spec(vec!["codex".to_string(), "claude-code".to_string()]);
        let yaml = serde_yml::to_string(&spec).expect("serialize");
        let parsed: PlacementPolicySpec = serde_yml::from_str(&yaml).expect("deserialize");
        assert_eq!(parsed, spec);
    }

    #[test]
    fn agent_adapters_defaults_to_empty_when_omitted_from_yaml() {
        let yaml = "pool: docker\ndocker_per_vessel:\n  host_ref: '01HXYZ'\n  image: ghcr.io/flotilla/dev:latest\n  checkout:\n    fresh_clone_in_container:\n      clone_path: /workspace\n";
        let parsed: PlacementPolicySpec = serde_yml::from_str(yaml).expect("deserialize");
        assert_eq!(parsed.docker_per_vessel.expect("docker_per_vessel").agent_adapters, Vec::<String>::new());
    }
}
