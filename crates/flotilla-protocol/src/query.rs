use std::{collections::HashMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    snapshot::{ProviderError, WorkItem},
    EnvironmentInfo, HostName, HostSummary, IssueSource, NodeInfo, PeerConnectionState, RepositoryKey, ViewAddress,
};

/// Provider health across categories. Outer key: category (e.g. "vcs",
/// "change_request"). Inner key: provider name. Value: healthy.
pub type ProviderHealthMap = HashMap<String, HashMap<String, bool>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewCommandContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convoy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vessel_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewListResponse {
    pub convoy: String,
    pub vessel_ref: String,
    pub vessel: String,
    pub members: Vec<CrewListMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewListMember {
    pub role: String,
    pub kind: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<String>,
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet_requirements: Vec<UnmetRequirementInfo>,
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

// --- project list ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectListEntry {
    pub namespace: String,
    pub name: String,
    pub display_name: String,
    pub address: ViewAddress,
    pub repositories: Vec<ProjectListRepository>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_source: Option<IssueSource>,
    pub default_workflow_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectListRepository {
    pub key: RepositoryKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

#[cfg(test)]
mod project_list_tests {
    use serde_json::json;

    use super::{ProjectListEntry, ProjectListRepository, ProjectListResponse};
    use crate::{IssueSource, RepositoryKey, ViewAddress};

    #[test]
    fn project_list_json_is_stable_and_typed() {
        let response = ProjectListResponse {
            projects: vec![ProjectListEntry::builder()
                .namespace("flotilla".to_string())
                .name("platform".to_string())
                .display_name("Platform".to_string())
                .address(ViewAddress::Project { namespace: "flotilla".into(), name: "platform".into() })
                .repositories(vec![ProjectListRepository {
                    key: RepositoryKey("repo-key".into()),
                    slug: Some("flotilla-org/flotilla".into()),
                }])
                .maybe_issue_source(Some(IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }))
                .default_workflow_ref("single-agent-contained".to_string())
                .build()],
        };

        assert_eq!(
            serde_json::to_value(response).expect("serialize"),
            json!({
                "projects": [{
                    "namespace": "flotilla",
                    "name": "platform",
                    "display_name": "Platform",
                    "address": "project/flotilla/platform",
                    "repositories": [{"key": "repo-key", "slug": "flotilla-org/flotilla"}],
                    "issue_source": {"service": "https://github.com", "scope": "flotilla-org/flotilla"},
                    "default_workflow_ref": "single-agent-contained"
                }]
            })
        );
    }
}

// --- fleet listing / replicas ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetListResponse {
    pub rows: Vec<FleetListRow>,
    #[serde(default)]
    pub replicas: Vec<FleetReplicaStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct FleetListRow {
    pub convoy: String,
    pub vessel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    pub crew: String,
    pub crew_state: String,
    pub host: HostName,
    /// Namespace the crew session lives in on its owning host.
    pub namespace: String,
    /// Crew session name — with `host` and `namespace` this is the
    /// `flotilla.session` pane join key. Absent for crewless convoy rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub staleness: FleetStaleness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetStaleness {
    Local,
    Fresh {
        last_sync: DateTime<Utc>,
    },
    Stale {
        last_sync: DateTime<Utc>,
    },
    Unreachable {
        #[serde(skip_serializing_if = "Option::is_none")]
        last_sync: Option<DateTime<Utc>>,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetReplicaStatus {
    pub host: HostName,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetReplicaSnapshot {
    pub host: HostName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    pub rows: Vec<FleetListRow>,
    /// This host's local result sets, one per named query — the unit of
    /// federated query union when replicas merge.
    #[serde(default)]
    pub result_sets: Vec<crate::result_set::ResultSet>,
}

// --- host / topology ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostListResponse {
    pub hosts: Vec<HostListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostListEntry {
    pub environment_id: crate::EnvironmentId,
    pub host_name: HostName,
    pub node: NodeInfo,
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
    pub environment_id: crate::EnvironmentId,
    pub host_name: HostName,
    pub node: NodeInfo,
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
    pub environment_id: crate::EnvironmentId,
    pub host_name: HostName,
    pub node: NodeInfo,
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
    pub local_node: NodeInfo,
    pub routes: Vec<TopologyRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyRoute {
    pub target: NodeInfo,
    pub next_hop: NodeInfo,
    pub direct: bool,
    pub connected: bool,
    #[serde(default)]
    pub fallbacks: Vec<NodeInfo>,
    /// Most recent dial attempt for this configured direct peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<DateTime<Utc>>,
    /// Error from the most recent failed dial. Cleared after a successful dial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        HostListEntry, HostListResponse, HostProvidersResponse, HostStatusResponse, TopologyResponse, TopologyRoute, UnmetRequirementInfo,
    };
    use crate::{
        qualified_path::HostId, test_helpers::assert_roundtrip, EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostEnvironment,
        HostName, HostProviderStatus, HostSummary, ImageId, NodeId, NodeInfo, PeerConnectionState, SystemInfo, ToolInventory,
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
            environment_id: EnvironmentId::host(HostId::new("desktop-host")),
            host_name: Some(HostName::new("desktop")),
            node: NodeInfo::new(NodeId::new("desktop"), "Desktop"),
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
                host_id: None,
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
                environment_id: EnvironmentId::host(HostId::new("remote-laptop-host")),
                host_name: HostName::new("remote-laptop"),
                node: NodeInfo::new(NodeId::new("node-remote-1"), "Remote Laptop"),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Disconnected,
                has_summary: false,
                repo_count: 0,
                work_item_count: 0,
            }],
        };

        let json = serde_json::to_value(&response).expect("serialize host list");
        assert_eq!(json["hosts"][0]["environment_id"], "host:remote-laptop-host");
        assert_eq!(json["hosts"][0]["node"]["node_id"], "node-remote-1");
        assert_eq!(json["hosts"][0]["node"]["display_name"], "Remote Laptop");
        assert_roundtrip(&response);
    }

    #[test]
    fn host_status_response_roundtrips_with_summary() {
        let response = HostStatusResponse {
            environment_id: EnvironmentId::host(HostId::new("desktop-host")),
            host_name: HostName::new("desktop"),
            node: NodeInfo::new(NodeId::new("node-desktop-1"), "Desktop Workstation"),
            is_local: true,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            summary: Some(sample_host_summary()),
            visible_environments: sample_visible_environments(),
            repo_count: 2,
            work_item_count: 5,
        };

        let json = serde_json::to_value(&response).expect("serialize host status");
        assert_eq!(json["environment_id"], "host:desktop-host");
        assert_eq!(json["node"]["node_id"], "node-desktop-1");
        assert_eq!(json["summary"]["node"]["display_name"], "Desktop");
        assert_roundtrip(&response);
    }

    #[test]
    fn host_providers_response_roundtrips_summary() {
        let response = HostProvidersResponse {
            environment_id: EnvironmentId::host(HostId::new("desktop-host")),
            host_name: HostName::new("desktop"),
            node: NodeInfo::new(NodeId::new("node-desktop-1"), "Desktop Workstation"),
            is_local: true,
            configured: true,
            connection_status: PeerConnectionState::Connected,
            summary: sample_host_summary(),
            visible_environments: sample_visible_environments(),
        };

        let json = serde_json::to_value(&response).expect("serialize host providers");
        assert_eq!(json["environment_id"], "host:desktop-host");
        assert_roundtrip(&response);
    }

    #[test]
    fn host_status_response_defaults_missing_visible_environments() {
        let mut value = serde_json::to_value(HostStatusResponse {
            environment_id: EnvironmentId::host(HostId::new("desktop-host")),
            host_name: HostName::new("desktop"),
            node: NodeInfo::new(NodeId::new("node-desktop-1"), "Desktop Workstation"),
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
            local_node: NodeInfo::new(NodeId::new("node-desktop-1"), "Desktop Workstation"),
            routes: vec![TopologyRoute {
                target: NodeInfo::new(NodeId::new("node-worker-1"), "Worker"),
                next_hop: NodeInfo::new(NodeId::new("node-relay-1"), "Relay"),
                direct: false,
                connected: true,
                fallbacks: vec![NodeInfo::new(NodeId::new("node-backup-relay-1"), "Backup Relay")],
                last_attempt: None,
                last_error: None,
            }],
        };

        assert_roundtrip(&response);
    }
}
