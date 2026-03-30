use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    snapshot::{ProviderError, WorkItem},
    EnvironmentInfo, HostName, HostSummary, PeerConnectionState,
};

/// Provider health across categories. Outer key: category (e.g. "vcs",
/// "change_request"). Inner key: provider name. Value: healthy.
pub type ProviderHealthMap = HashMap<String, HashMap<String, bool>>;

// --- status ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub repos: Vec<RepoSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSummary {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_item_count: usize,
    pub error_count: usize,
}

// --- repo detail ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDetailResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_items: Vec<WorkItem>,
    pub errors: Vec<ProviderError>,
}

// --- repo providers ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoProvidersResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub host_discovery: Vec<DiscoveryEntry>,
    pub repo_discovery: Vec<DiscoveryEntry>,
    pub providers: Vec<ProviderInfo>,
    pub unmet_requirements: Vec<UnmetRequirementInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryEntry {
    pub kind: String,
    pub detail: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub category: String,
    pub name: String,
    pub healthy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmetRequirementInfo {
    pub factory: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

// --- repo work ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoWorkResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub work_items: Vec<WorkItem>,
}

// --- host / topology ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostListResponse {
    pub hosts: Vec<HostListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostListEntry {
    pub host: HostName,
    pub is_local: bool,
    /// `true` only for non-local hosts that appear in `hosts.toml`.
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    /// Indicates whether `get_host_status` would be able to return a
    /// non-`None` summary for this host.
    pub has_summary: bool,
    pub repo_count: usize,
    pub work_item_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatusResponse {
    pub host: HostName,
    pub is_local: bool,
    /// `true` only for non-local hosts that appear in `hosts.toml`.
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<HostSummary>,
    #[serde(default)]
    pub visible_environments: Vec<EnvironmentInfo>,
    pub repo_count: usize,
    pub work_item_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProvidersResponse {
    pub host: HostName,
    pub is_local: bool,
    /// `true` only for non-local hosts that appear in `hosts.toml`.
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    pub summary: HostSummary,
    #[serde(default)]
    pub visible_environments: Vec<EnvironmentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyResponse {
    pub local_host: HostName,
    pub routes: Vec<TopologyRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyRoute {
    pub target: HostName,
    pub next_hop: HostName,
    pub direct: bool,
    pub connected: bool,
    #[serde(default)]
    pub fallbacks: Vec<HostName>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        HostListEntry, HostListResponse, HostProvidersResponse, HostStatusResponse, TopologyResponse, TopologyRoute, UnmetRequirementInfo,
    };
    use crate::{
        test_helpers::assert_roundtrip, EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostEnvironment, HostName, HostProviderStatus,
        HostSummary, ImageId, PeerConnectionState, SystemInfo, ToolInventory,
    };

    #[test]
    fn unmet_requirement_info_omits_none_value_when_serialized() {
        let without_value = UnmetRequirementInfo { factory: "git".into(), kind: "no_vcs_checkout".into(), value: None };
        let with_value = UnmetRequirementInfo { factory: "github".into(), kind: "missing_binary".into(), value: Some("gh".into()) };

        assert_eq!(
            serde_json::to_value(&without_value).expect("serialize without value"),
            json!({
                "factory": "git",
                "kind": "no_vcs_checkout"
            })
        );
        assert_eq!(
            serde_json::to_value(&with_value).expect("serialize with value"),
            json!({
                "factory": "github",
                "kind": "missing_binary",
                "value": "gh"
            })
        );
    }

    fn sample_host_summary() -> HostSummary {
        HostSummary {
            host_name: HostName::new("desktop"),
            system: SystemInfo {
                home_dir: Some("/home/dev".into()),
                os: Some("linux".into()),
                arch: Some("aarch64".into()),
                cpu_count: Some(8),
                memory_total_mb: Some(16384),
                environment: HostEnvironment::Container,
            },
            inventory: ToolInventory::default(),
            providers: vec![HostProviderStatus { category: "vcs".into(), name: "Git".into(), implementation: "git".into(), healthy: true }],
            environments: vec![],
        }
    }

    fn sample_visible_environments() -> Vec<EnvironmentInfo> {
        vec![
            EnvironmentInfo::Direct {
                id: EnvironmentId::new("direct-env"),
                display_name: Some("direct".into()),
                status: EnvironmentStatus::Running,
            },
            EnvironmentInfo::Provisioned {
                id: EnvironmentId::new("provisioned-env"),
                display_name: Some("provisioned".into()),
                image: ImageId::new("mock:image"),
                status: EnvironmentStatus::Running,
            },
        ]
    }

    #[test]
    fn host_list_response_roundtrips_without_summary_data() {
        let response = HostListResponse {
            hosts: vec![HostListEntry {
                host: HostName::new("remote"),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Disconnected,
                has_summary: false,
                repo_count: 0,
                work_item_count: 0,
            }],
        };

        assert_roundtrip(&response);
    }

    #[test]
    fn host_status_response_roundtrips_with_summary() {
        let response = HostStatusResponse {
            host: HostName::new("desktop"),
            is_local: true,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            summary: Some(sample_host_summary()),
            visible_environments: sample_visible_environments(),
            repo_count: 2,
            work_item_count: 5,
        };

        assert_roundtrip(&response);
    }

    #[test]
    fn host_providers_response_roundtrips_summary() {
        let response = HostProvidersResponse {
            host: HostName::new("desktop"),
            is_local: true,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            summary: sample_host_summary(),
            visible_environments: sample_visible_environments(),
        };

        assert_roundtrip(&response);
    }

    #[test]
    fn host_status_response_defaults_missing_visible_environments() {
        let mut value = serde_json::to_value(HostStatusResponse {
            host: HostName::new("desktop"),
            is_local: true,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            summary: Some(sample_host_summary()),
            visible_environments: vec![],
            repo_count: 2,
            work_item_count: 5,
        })
        .expect("serialize host status");
        value.as_object_mut().expect("object").remove("visible_environments");

        let decoded: HostStatusResponse = serde_json::from_value(value).expect("deserialize without visible environments");
        assert!(decoded.visible_environments.is_empty());
    }

    #[test]
    fn topology_response_roundtrips_fallbacks() {
        let response = TopologyResponse {
            local_host: HostName::new("desktop"),
            routes: vec![TopologyRoute {
                target: HostName::new("worker"),
                next_hop: HostName::new("relay"),
                direct: false,
                connected: true,
                fallbacks: vec![HostName::new("backup-relay")],
            }],
        };

        assert_roundtrip(&response);
    }
}
