use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{EnvironmentInfo, HostName, HostSummary, IssueSource, NodeInfo, PeerConnectionState, RepositoryKey, ViewAddress};

/// Provider health across categories. Outer key: category (e.g. "vcs",
/// "change_request"). Inner key: provider name. Value: healthy.
pub type ProviderHealthMap = HashMap<String, HashMap<String, bool>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchQueueResponse {
    pub observed_at: DateTime<Utc>,
    pub entries: Vec<DispatchQueueRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchQueueRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<crate::DispatchScore>,
    pub namespace: String,
    pub project: String,
    pub issue: crate::IssueRef,
    pub title: String,
    pub ready_observed_at: DateTime<Utc>,
    pub age_seconds: u64,
    pub attention: bool,
    pub provenance: String,
}

/// Tracker facts for a board, obtained through the daemon's source adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchBoardResponse {
    pub readiness: DispatchQueueResponse,
    pub repositories: Vec<DispatchBoardRepository>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchBoardRepository {
    /// Completion time of the last successful tracker observation.
    pub observed_at: DateTime<Utc>,
    pub age_seconds: u64,
    /// A failed refresh does not discard previously observed facts.
    pub refresh_error: Option<String>,
    pub source: IssueSource,
    pub issues: Vec<DispatchBoardIssue>,
    pub pull_requests: Vec<DispatchBoardPullRequest>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchBoardIssue {
    pub id: String,
    pub title: String,
    pub state: crate::IssueState,
    pub url: String,
    pub updated_at: String,
    pub parent: Option<crate::IssueRef>,
    pub issue_type: Option<String>,
    #[builder(default)]
    pub mission_fields: crate::MissionFields,
    pub closed_at: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<DispatchBoardDependency>,
    pub pull_requests: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchBoardDependency {
    pub url: String,
    pub state: crate::IssueState,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchBoardPullRequest {
    pub id: String,
    pub url: String,
    pub state: String,
    pub merged_at: Option<String>,
    pub merge_state: Option<String>,
    pub ci: String,
}

/// A result-row identity must retain Project scope even when two Projects
/// contain the same external issue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DispatchReadyKey {
    pub namespace: String,
    pub project: String,
    pub issue: crate::IssueRef,
}
impl DispatchQueueRow {
    pub fn key(&self) -> DispatchReadyKey {
        DispatchReadyKey { namespace: self.namespace.clone(), project: self.project.clone(), issue: self.issue.clone() }
    }
}

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
    /// Charter resolved from the live Project on every crew query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<CrewProject>,
    /// An unavailable live charter, kept separate from process state and alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub credential_alerts: Vec<String>,
}

/// Durable inbox projection shared by crew and convoy views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewMessageView {
    pub current_receiver: Option<String>,
    pub name: String,
    pub sender: String,
    pub receiver: String,
    pub relation: String,
    pub phase: String,
    pub since: String,
    pub reason: Option<String>,
    pub subject: Option<serde_json::Value>,
    pub expectation: serde_json::Value,
    pub crew_id: Option<String>,
    pub session: Option<String>,
}

/// Current island membership, independent of the convoy's admission snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewProject {
    pub namespace: String,
    pub name: String,
    pub display_name: String,
    pub repositories: Vec<CrewProjectRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewProjectRepository {
    pub key: RepositoryKey,
    pub alias: Option<String>,
    pub roles: BTreeSet<ProjectRepositoryRole>,
    pub subpath: Option<String>,
    pub default_branch: Option<String>,
    /// Live repository remotes; empty when the Repository is unavailable.
    pub remotes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRepositoryRole {
    Code,
    Ops,
    Knowledge,
}

impl std::fmt::Display for ProjectRepositoryRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Code => "code",
            Self::Ops => "ops",
            Self::Knowledge => "knowledge",
        })
    }
}

#[cfg(test)]
mod project_repository_role_tests {
    use super::ProjectRepositoryRole;

    // Human-readable roles use the same names as the serialized charter.
    #[test]
    fn display_matches_serialized_role_names() {
        for role in [ProjectRepositoryRole::Code, ProjectRepositoryRole::Ops, ProjectRepositoryRole::Knowledge] {
            assert_eq!(serde_json::to_value(role).expect("role serializes"), role.to_string());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewListMember {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub messages: Vec<CrewMessageView>,
    pub role: String,
    pub kind: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<CrewAttention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<String>,
}

/// Live monitoring state for crew work, kept separate from terminal and
/// workflow lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrewAttention {
    Working,
    NeedsInput,
    DeliveryUnconfirmed,
    Idle,
    Unobservable,
}

impl std::fmt::Display for CrewAttention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Working => "working",
            Self::NeedsInput => "needs input",
            Self::DeliveryUnconfirmed => "delivery unconfirmed",
            Self::Idle => "idle",
            Self::Unobservable => "unobservable",
        })
    }
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet_requirements: Vec<UnmetRequirementInfo>,
}

// --- repo providers ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoProvidersResponse {
    pub repository: crate::RepositoryKey,
    pub path: Option<PathBuf>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmetRequirementInfo {
    pub factory: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

// --- project list ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectListEntry {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[builder(default)]
    pub is_fleet: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration_refused: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub declaration_stale: bool,
    pub namespace: String,
    pub name: String,
    pub display_name: String,
    pub address: ViewAddress,
    pub repositories: Vec<ProjectListRepository>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_sources: Vec<IssueSource>,
    pub default_workflow_ref: String,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectListRepository {
    pub key: RepositoryKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subpaths: Vec<String>,
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
                    subpaths: vec![],
                }])
                .issue_sources(vec![IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }])
                .default_workflow_ref("single-agent".to_string())
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
                    "issue_sources": [{"service": "https://github.com", "scope": "flotilla-org/flotilla"}],
                    "default_workflow_ref": "single-agent"
                }]
            })
        );
    }
}

// --- fleet listing / replicas ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetHealthResponse {
    pub hosts: Vec<FleetHostRow>,
    #[serde(default)]
    pub forge_budgets: Vec<ForgeBudgetRow>,
    #[serde(default)]
    pub dispatch_queue: DispatchQueueResponse,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentListResponse {
    pub kinds: Vec<FulfilmentRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentRow {
    pub name: String,
    pub host_ref: String,
    pub pool: String,
    pub realisation: String,
    pub grants: Vec<String>,
    #[serde(default)]
    pub harnesses: BTreeMap<String, FulfilmentHarness>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, String>,
    pub gui_session_logged_in: Option<bool>,
    pub free_vessel_slots: Option<u32>,
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentHarness {
    pub version: String,
    pub models: BTreeMap<String, FulfilmentModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentModel {
    pub usable: bool,
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SleepInhibitionHealth {
    #[default]
    NotRequired,
    Held,
    Acquiring {
        consecutive_failures: u32,
        message: String,
    },
    Failed {
        consecutive_failures: u32,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct FleetHostRow {
    pub host: HostName,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fulfilments: Vec<FulfilmentRow>,
    pub is_local: bool,
    pub configured: bool,
    pub link: PeerConnectionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_generation: Option<String>,
    /// Wire-generation fingerprint compared by the daemon handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_uptime_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_last_sync: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_generation: Option<String>,
    pub crew_count: usize,
    pub convoy_count: usize,
    #[builder(default)]
    #[serde(default)]
    pub surface_states: FleetSurfaceCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_sync: Option<crate::BlobSyncStatus>,
    #[serde(default)]
    #[builder(default)]
    pub sleep_inhibition: SleepInhibitionHealth,
    pub staleness: FleetHostStaleness,
    pub observation_agreement: FleetObservationAgreement,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_conditions: Vec<String>,
    /// Expired or near-expiry credential material on this host, one entry per
    /// affected scope ("ambient claude login expired on 2026-07-30").
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_attention: Vec<CredentialAttention>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSurfaceCounts {
    pub available: usize,
    pub stalled_handled: usize,
    pub needs_you: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSyncStatus {
    pub pending_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialAttention {
    pub severity: CredentialAttentionSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAttentionSeverity {
    /// Material is past its effective expiry; dependent dispatch is refused.
    Expired,
    /// Material expires within the host's warning window.
    Expiring,
    /// The published expiry capability could not be decoded.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetHostStaleness {
    Current,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetObservationAgreement {
    Agree,
    Disagree,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetListResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_project: Option<crate::ResourceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declaration_attention: Vec<DeclarationAttentionRow>,
    pub rows: Vec<FleetListRow>,
    #[serde(default)]
    pub replicas: Vec<FleetReplicaStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationAttentionRow {
    pub resource: crate::ResourceRef,
    pub condition: DeclarationAttentionKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclarationAttentionKind {
    DeclarationRefused,
    ConfigDrift,
}

impl std::fmt::Display for DeclarationAttentionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::DeclarationRefused => "DeclarationRefused",
            Self::ConfigDrift => "ConfigDrift",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct FleetListRow {
    pub convoy: String,
    /// Opaque record key used for internal joins. Human surfaces render
    /// `convoy`, which is the role address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convoy_ref: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<crate::result_set::ConvoySubjectRow>,
    pub vessel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    pub crew: String,
    pub crew_state: String,
    #[builder(default)]
    #[serde(default)]
    pub surface_state: crate::result_set::SurfaceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<CrewAttention>,
    pub host: HostName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<crate::PlacementDecision>,
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

// --- host / topology ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostListResponse {
    pub hosts: Vec<HostListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostListEntry {
    /// Canonical host environment identity, when the peer has supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<crate::EnvironmentId>,
    pub host_name: HostName,
    /// Canonical node identity, when the configured target has connected at
    /// least once or pins an expected node id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeInfo>,
    pub is_local: bool,
    /// `true` only for non-local hosts that appear in `hosts.toml`.
    pub configured: bool,
    pub connection_status: PeerConnectionState,
    /// Live redial details when `connection_status` is `Reconnecting`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<PeerReconnectStatus>,
    /// Indicates whether `get_host_status` would be able to return a
    /// non-`None` summary for this host.
    pub has_summary: bool,
    pub repo_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReconnectStatus {
    pub attempt: u32,
    pub next_dial_in_seconds: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_sync: Option<BlobSyncStatus>,
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

/// Escalation rung shared by stored convoy conditions and query rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallRung {
    Nudge,
    Supervisor,
    Bosun,
    Governor,
    Operator,
}

impl std::fmt::Display for StallRung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nudge => "nudge",
            Self::Supervisor => "supervisor",
            Self::Bosun => "bosun",
            Self::Governor => "governor",
            Self::Operator => "operator",
        })
    }
}

/// Fleet-wide stalled obligations; evidence remains complete in JSON output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewStallsResponse {
    pub observed_at: DateTime<Utc>,
    pub full: bool,
    pub rows: Vec<CrewStallRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewStallRow {
    pub namespace: String,
    pub project: Option<String>,
    pub project_display_name: Option<String>,
    pub convoy: String,
    pub convoy_display_name: String,
    pub vessel: String,
    pub role: String,
    pub rung: Option<StallRung>,
    pub supervisor: Option<String>,
    pub supervisor_absence_reason: Option<String>,
    pub began_at: Option<DateTime<Utc>>,
    pub age_seconds: Option<u64>,
    pub proposed_disposition: Option<crate::StallProposedDisposition>,
    pub evidence: String,
    /// Equal keys flag possible shared causes, without guessing from keywords.
    pub cause_group: String,
    pub shared_cause_count: usize,
    pub artifacts: Vec<String>,
}

#[cfg(test)]
mod tests {
    // Display labels preserve the wire spelling for every rung and proposed disposition.
    #[test]
    fn stall_labels_match_wire_values() {
        for rung in [
            super::StallRung::Nudge,
            super::StallRung::Supervisor,
            super::StallRung::Bosun,
            super::StallRung::Governor,
            super::StallRung::Operator,
        ] {
            assert_eq!(rung.to_string(), serde_json::to_value(rung).expect("rung serializes").as_str().expect("string"));
        }
        for disposition in
            [crate::StallProposedDisposition::Resume, crate::StallProposedDisposition::ReduceScope, crate::StallProposedDisposition::Fail]
        {
            assert_eq!(
                disposition.to_string(),
                serde_json::to_value(disposition).expect("disposition serializes").as_str().expect("string")
            );
        }
    }

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
            providers: vec![HostProviderStatus {
                category: "vcs".into(),
                name: "Git".into(),
                implementation: "git".into(),
                healthy: true,
                disabled_reason: None,
            }],
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
                environment_id: Some(EnvironmentId::host(HostId::new("remote-laptop-host"))),
                host_name: HostName::new("remote-laptop"),
                node: Some(NodeInfo::new(NodeId::new("node-remote-1"), "Remote Laptop")),
                is_local: false,
                configured: true,
                connection_status: PeerConnectionState::Disconnected,
                reconnect: None,
                has_summary: false,
                repo_count: 0,
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
            blob_sync: None,
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
            blob_sync: None,
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

/// Actual reported cost is separate from calls whose cost the CLI hides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeBudgetRow {
    pub host: String,
    pub identity: String,
    pub budget: String,
    pub window_start: chrono::DateTime<chrono::Utc>,
    pub calls: u64,
    pub reported_cost: u64,
    pub unreported_calls: u64,
    pub remaining: Option<u64>,
    pub retry_at: Option<chrono::DateTime<chrono::Utc>>,
}
