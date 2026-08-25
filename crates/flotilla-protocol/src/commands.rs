use std::path::PathBuf;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

use crate::{
    arg::Arg,
    issue_query::{IssueQuery, IssueResultPage},
    qualified_path::QualifiedPath,
    query::{
        CrewCommandContext, CrewListResponse, DispatchQueueResponse, FleetHealthResponse, FleetListResponse, FleetReplicaSnapshot,
        HostListResponse, HostProvidersResponse, HostStatusResponse, ProjectListResponse, RepoProvidersResponse,
    },
    AttachableSetId, IssueRef, RepoIdentity,
};

fn is_false(value: &bool) -> bool {
    !*value
}
#[cfg(test)]
use crate::{qualified_path::HostId, EnvironmentId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoSelector {
    Path(PathBuf),
    Query(String),
    Identity(RepoIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutSelector {
    Path(PathBuf),
    Query(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutTarget {
    Branch(String),
    FreshBranch(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedTerminalCommand {
    pub role: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ResourceJsonResponse {
    #[serde(rename = "resourceKind")]
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "replicaOrigin")]
    pub replica_origin: Option<crate::NodeId>,
}

/// Opaque position in one resource kind's ordered mutation stream.
///
/// The encoded form is stable for scripts but deliberately hides backend
/// resource versions and generations from the client surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceCursor(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResourceCursorPayload {
    version: u8,
    resource_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<String>,
}

impl ResourceCursor {
    pub fn from_position(resource_version: impl Into<String>, generation: Option<String>) -> Self {
        let payload = ResourceCursorPayload { version: 1, resource_version: resource_version.into(), generation };
        let encoded = serde_json::to_vec(&payload).expect("resource cursor payload is serializable");
        Self(URL_SAFE_NO_PAD.encode(encoded))
    }

    pub fn position(&self) -> Result<(String, Option<String>), String> {
        let decoded = URL_SAFE_NO_PAD.decode(&self.0).map_err(|error| format!("invalid resource cursor encoding: {error}"))?;
        let payload: ResourceCursorPayload =
            serde_json::from_slice(&decoded).map_err(|error| format!("invalid resource cursor payload: {error}"))?;
        if payload.version != 1 {
            return Err(format!("unsupported resource cursor version {}", payload.version));
        }
        Ok((payload.resource_version, payload.generation))
    }
}

impl std::fmt::Display for ResourceCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for ResourceCursor {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let cursor = Self(value.to_string());
        cursor.position()?;
        Ok(cursor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ResourceRecordType {
    Current,
    Added,
    Modified,
    Deleted,
    Bookmark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ResourceRecordProvenance {
    Local {
        #[serde(rename = "nodeId")]
        node_id: crate::NodeId,
    },
    Replica {
        #[serde(rename = "originRoot")]
        origin_root: crate::NodeId,
        #[serde(rename = "lastSyncedAt")]
        last_synced_at: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestResolution {
    Sync,
    Adopt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceReadRecord {
    #[serde(rename = "type")]
    pub record_type: ResourceRecordType,
    pub provenance: ResourceRecordProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<serde_json::Value>,
}

/// Stable envelope returned by resource get/list/watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ResourceReadEnvelope {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    #[serde(rename = "resourceKind")]
    pub resource_kind: String,
    pub plural: String,
    pub namespace: String,
    pub cursor: ResourceCursor,
    pub records: Vec<ResourceReadRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFreshness {
    Fresh,
    Stale,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutArchiveStatus {
    Archived,
    NothingToArchive,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CheckoutArchiveOutcome {
    pub checkout: String,
    pub status: CheckoutArchiveStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedCondition {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    pub freshness: EvidenceFreshness,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedCheckout {
    pub name: String,
    pub observed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ResourceRecordProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<ExplainedCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<ExplainedCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed: Option<ExplainedCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedChangeRequest {
    pub name: String,
    pub bound: bool,
    pub observed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ResourceRecordProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    pub freshness: EvidenceFreshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedLeafFiring {
    pub leaf: crate::Leaf,
    pub value: String,
    pub fired_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedSubscription {
    pub id: uuid::Uuid,
    pub watcher: String,
    pub leaves: Vec<crate::Leaf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_leaf_firings: Vec<ExplainedLeafFiring>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedCrewDelivery {
    pub session: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_rung: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ExplainedDecisionLedger {
    pub vessel: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_url: Option<String>,
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedUnmetExpectation {
    pub reason: String,
    pub subject: String,
    pub detail: String,
}

pub const SETTLEMENT_MODE_STANDING: &str = "standing";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainedSettlement {
    pub mode: String,
    pub satisfied: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet: Vec<ExplainedUnmetExpectation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyExplanation {
    pub namespace: String,
    pub convoy: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub evidence_ttl_seconds: u64,
    pub change_request_stale_after_seconds: u64,
    pub checkouts: Vec<ExplainedCheckout>,
    pub change_requests: Vec<ExplainedChangeRequest>,
    pub subscriptions: Vec<ExplainedSubscription>,
    pub crew_deliveries: Vec<ExplainedCrewDelivery>,
    pub decision_ledgers: Vec<ExplainedDecisionLedger>,
    pub settlement: ExplainedSettlement,
}

/// Filters for reading a daemon's host-local structured log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DaemonLogQuery {
    /// Only include events this many seconds old or newer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seconds: Option<u64>,
    /// Minimum tracing level (`trace`, `debug`, `info`, `warn`, or `error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// Exact tracing target or one of its child module paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Structured resolved attach command for a workspace pane.
/// Produced on the target host, consumed on the presentation host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPaneCommand {
    pub role: String,
    pub args: Vec<Arg>,
}

/// Execution-side workspace preparation artifact.
/// Produced on the checkout host and consumed by the presentation-host attach step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedWorkspace {
    pub label: String,
    pub target_node_id: crate::NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_host: Option<crate::HostName>,
    pub checkout_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_key: Option<QualifiedPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachable_set_id: Option<AttachableSetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<crate::EnvironmentId>,
    /// Provider-specific transport handle (e.g. Docker container name).
    /// Set by PrepareWorkspace on the remote daemon, consumed by AttachWorkspace
    /// on the presentation host for hop chain construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_yaml: Option<String>,
    pub prepared_commands: Vec<ResolvedPaneCommand>,
}

/// Routed command envelope shared by all frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct Command {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<crate::NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioning_target: Option<crate::ProvisioningTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_repo: Option<RepoSelector>,
    #[serde(flatten)]
    pub action: CommandAction,
}

/// One issue supplied to convoy admission either as a fully source-qualified
/// reference or as an opaque ID whose source must be resolved through the
/// Project. A start intent may carry several selectors.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum IssueSelector {
    Id(String),
    Alias { alias: String, id: String },
    Reference(IssueRef),
}

/// Partial intent accepted by the convoy start verb. Admission completes any
/// omitted fields before persisting a `ConvoySpec`.
/// `issues` contains zero or more issue selectors to snapshot into the
/// admitted convoy.
///
/// This type lives in protocol rather than resources because incomplete
/// intent must never enter the resource store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvoyAutoAttach {
    /// Let the dispatching daemon choose from configuration and connected
    /// presentation-surface presence.
    #[default]
    Default,
    /// Attach even when a presentation surface is connected.
    Always,
    /// Leave the convoy latent even when no presentation surface is connected.
    Never,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    /// Request control, degrading to a read-only watcher when control is held.
    #[default]
    Default,
    /// Take control when the terminal pool supports controller seats, otherwise attach normally.
    PreferTake,
    /// Refuse when another attachment holds control.
    Strict,
    /// Take control and demote the current controller to watcher.
    Take,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvoyDispatchRegard {
    #[default]
    Emit,
    Suppress,
}

impl From<ConvoyAutoAttach> for ConvoyDispatchRegard {
    fn from(auto_attach: ConvoyAutoAttach) -> Self {
        match auto_attach {
            ConvoyAutoAttach::Never => Self::Suppress,
            ConvoyAutoAttach::Default | ConvoyAutoAttach::Always => Self::Emit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyStartIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub project_ref: String,
    /// Existing change request to adopt. Admission resolves all remaining
    /// checkout identity from this provider-native ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<IssueSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_ref: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy: Option<String>,
    /// Dispatch-time agent requirement overrides, applied to the workflow
    /// snapshot's capability selectors at admission (`--agent`).
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_overrides: Vec<AgentOverride>,
    #[builder(default)]
    #[serde(default)]
    pub auto_attach: ConvoyAutoAttach,
}

/// One dispatch-time agent choice: which harness (and optionally model) the
/// named capability resolves to for this convoy only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOverride {
    pub capability: String,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// A convoy launch admitted by the presentation host and ready to be
/// persisted by the selected execution host.
///
/// Commands the client can send to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CommandAction {
    CreateWorkspaceForCheckout {
        checkout_path: PathBuf,
        label: String,
    },
    CreateWorkspaceFromPreparedTerminal {
        target_node_id: crate::NodeId,
        branch: String,
        checkout_path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachable_set_id: Option<AttachableSetId>,
        commands: Vec<ResolvedPaneCommand>,
    },
    SelectWorkspace {
        ws_ref: String,
    },
    Attach {
        reference: String,
        /// Restrict resolution to the host that advertised the recipe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<crate::HostName>,
        #[serde(default)]
        mode: AttachMode,
    },
    /// Resolve an attach for a temporary foreground excursion. Unlike the
    /// human-facing CLI attach, recursive hops must not stamp PM metadata.
    AttachTransient {
        reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<crate::HostName>,
        #[serde(default)]
        mode: AttachMode,
    },
    PrepareTerminalForCheckout {
        checkout_path: PathBuf,
        /// Role→command mappings from the requesting host's template.
        /// When non-empty, the remote side wraps these through its terminal pool
        /// instead of reading its own template.
        #[serde(default)]
        commands: Vec<PreparedTerminalCommand>,
    },
    Checkout {
        repo: RepoSelector,
        target: CheckoutTarget,
        #[serde(default)]
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        checkout: CheckoutSelector,
    },
    FetchCheckoutStatus {
        branch: String,
        checkout_path: Option<PathBuf>,
        change_request_id: Option<String>,
    },
    OpenChangeRequest {
        id: String,
    },
    CloseChangeRequest {
        id: String,
    },
    MergeChangeRequest {
        id: String,
        #[serde(default, skip_serializing_if = "is_false")]
        confirmed: bool,
    },
    OpenIssue {
        id: String,
    },
    LinkIssuesToChangeRequest {
        change_request_id: String,
        issue_ids: Vec<String>,
    },
    ArchiveSession {
        session_id: String,
    },
    GenerateBranchName {
        issue_keys: Vec<String>,
    },
    ConvoyWorkForceComplete {
        convoy: String,
        work: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    ConvoyDelete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    ConvoyAbandon {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
        reason: String,
    },
    ConvoyResume {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vessel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    ConvoyWithdrawPendingBrief {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
    },
    CrewHandoff {
        context: CrewCommandContext,
        target: String,
        message: String,
    },
    CrewComplete {
        context: CrewCommandContext,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision_ledger_ref: Option<String>,
    },
    CrewFail {
        context: CrewCommandContext,
        message: String,
    },
    ConvoyCreate {
        name: String,
        workflow_ref: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository_url: Option<String>,
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placement_policy: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adopted_checkout: Option<Box<PathBuf>>,
    },
    ConvoyStart {
        intent: Box<ConvoyStartIntent>,
    },
    WorkflowTemplateApply {
        name: String,
        spec_yaml: String,
    },
    ProjectAdd {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<String>,
    },
    ProjectApply {
        name: String,
        spec_yaml: String,
    },
    ProjectRegister {
        target: String,
    },
    ProjectRefresh {
        name: String,
    },
    TeleportSession {
        session_id: String,
        branch: Option<String>,
        checkout_key: Option<PathBuf>,
    },
    TrackRepoPath {
        path: PathBuf,
    },
    UntrackRepo {
        repo: RepoSelector,
    },
    RepositoryRemoteRemove {
        namespace: String,
        name: String,
        remote: String,
    },
    Refresh {
        repo: Option<RepoSelector>,
    },
    QueryIssues {
        repo: RepoSelector,
        params: IssueQuery,
        page: u32,
        count: usize,
    },
    QueryIssueFetchByIds {
        repo: RepoSelector,
        ids: Vec<String>,
    },
    QueryIssueOpenInBrowser {
        repo: RepoSelector,
        id: String,
    },
    // Query commands — read-only operations dispatched through execute()
    QueryRepoProviders {
        repo: RepoSelector,
    },
    QueryHostList {},
    QueryProjectList {},
    QueryDispatchQueue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    QueryHostStatus {
        target_environment_id: crate::EnvironmentId,
    },
    QueryHostProviders {
        target_environment_id: crate::EnvironmentId,
    },
    QueryFleetHealth {},
    QueryFleetList {},
    QueryCrewList {
        context: CrewCommandContext,
    },
    QueryFleetReplicaSnapshot {},
    QueryDaemonLogs {
        query: DaemonLogQuery,
    },
    QueryExplainConvoy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
    },
    QueryResourceList {
        namespace: String,
        kind: String,
        #[serde(default, skip_serializing_if = "is_false")]
        include_replicas: bool,
    },
    QueryResourceGet {
        namespace: String,
        kind: String,
        name: String,
    },
    ResourceApply {
        namespace: String,
        document: serde_json::Value,
    },
    ResourceManifestResolve {
        namespace: String,
        kind: String,
        name: String,
        resolution: ManifestResolution,
    },
    ResourceStatusPatch {
        namespace: String,
        kind: String,
        name: String,
        status: serde_json::Value,
    },
    ResourceDelete {
        namespace: String,
        kind: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replica_origin: Option<crate::NodeId>,
    },
    ResourceWatch {
        namespace: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        include_replicas: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        replica_sources: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<ResourceCursor>,
    },
}

impl CommandAction {
    /// Whether this action is a read-only query command.
    pub fn is_query(&self) -> bool {
        matches!(
            self,
            CommandAction::QueryRepoProviders { .. }
                | CommandAction::QueryHostList {}
                | CommandAction::QueryProjectList {}
                | CommandAction::QueryDispatchQueue { .. }
                | CommandAction::QueryHostStatus { .. }
                | CommandAction::QueryHostProviders { .. }
                | CommandAction::QueryFleetHealth {}
                | CommandAction::QueryFleetList {}
                | CommandAction::QueryCrewList { .. }
                | CommandAction::QueryFleetReplicaSnapshot {}
                | CommandAction::QueryDaemonLogs { .. }
                | CommandAction::QueryExplainConvoy { .. }
                | CommandAction::QueryResourceList { .. }
                | CommandAction::QueryResourceGet { .. }
                | CommandAction::Attach { .. }
                | CommandAction::AttachTransient { .. }
                | CommandAction::QueryIssues { .. }
                | CommandAction::QueryIssueFetchByIds { .. }
                | CommandAction::QueryIssueOpenInBrowser { .. }
        )
    }
}

impl Command {
    pub fn description(&self) -> &'static str {
        match &self.action {
            CommandAction::CreateWorkspaceForCheckout { .. } => "Creating workspace...",
            CommandAction::CreateWorkspaceFromPreparedTerminal { .. } => "Creating workspace...",
            CommandAction::SelectWorkspace { .. } => "Switching workspace...",
            CommandAction::Attach { .. } => "Resolving attach target...",
            CommandAction::AttachTransient { .. } => "Resolving temporary attach target...",
            CommandAction::PrepareTerminalForCheckout { .. } => "Preparing terminal...",
            CommandAction::Checkout { target, .. } => match target {
                CheckoutTarget::Branch(_) => "Checking out branch...",
                CheckoutTarget::FreshBranch(_) => "Creating checkout...",
            },
            CommandAction::RemoveCheckout { .. } => "Removing checkout...",
            CommandAction::FetchCheckoutStatus { .. } => "Fetching checkout status...",
            CommandAction::OpenChangeRequest { .. } => "Opening in browser...",
            CommandAction::CloseChangeRequest { .. } => "Closing PR...",
            CommandAction::MergeChangeRequest { .. } => "Merging change request...",
            CommandAction::OpenIssue { .. } => "Opening in browser...",
            CommandAction::LinkIssuesToChangeRequest { .. } => "Linking issues...",
            CommandAction::ArchiveSession { .. } => "Archiving session...",
            CommandAction::GenerateBranchName { .. } => "Generating branch name...",
            CommandAction::ConvoyWorkForceComplete { .. } => "Force-completing work...",
            CommandAction::ConvoyDelete { .. } => "Deleting convoy...",
            CommandAction::ConvoyAbandon { .. } => "Abandoning convoy...",
            CommandAction::ConvoyResume { .. } => "Resuming convoy crew...",
            CommandAction::ConvoyWithdrawPendingBrief { .. } => "Withdrawing pending convoy brief...",
            CommandAction::CrewHandoff { .. } => "Handing off to crew member...",
            CommandAction::CrewComplete { .. } => "Completing crew work...",
            CommandAction::CrewFail { .. } => "Failing crew work...",
            CommandAction::ConvoyCreate { .. } => "Creating convoy...",
            CommandAction::ConvoyStart { .. } => "Starting convoy...",
            CommandAction::WorkflowTemplateApply { .. } => "Applying workflow template...",
            CommandAction::ProjectAdd { .. } => "Adding project...",
            CommandAction::ProjectApply { .. } => "Applying project...",
            CommandAction::ProjectRegister { .. } => "Registering project declaration...",
            CommandAction::ProjectRefresh { .. } => "Refreshing project declaration...",
            CommandAction::TeleportSession { .. } => "Teleporting session...",
            CommandAction::TrackRepoPath { .. } => "Tracking repository...",
            CommandAction::UntrackRepo { .. } => "Untracking repository...",
            CommandAction::RepositoryRemoteRemove { .. } => "Removing repository remote...",
            CommandAction::Refresh { .. } => "Refreshing...",
            CommandAction::QueryIssues { .. } => "query issues",
            CommandAction::QueryIssueFetchByIds { .. } => "query issue fetch by ids",
            CommandAction::QueryIssueOpenInBrowser { .. } => "query issue open in browser",
            CommandAction::QueryRepoProviders { .. } => "query repo providers",
            CommandAction::QueryHostList {} => "query host list",
            CommandAction::QueryProjectList {} => "query project list",
            CommandAction::QueryDispatchQueue { .. } => "query dispatch queue",
            CommandAction::QueryHostStatus { .. } => "query host status",
            CommandAction::QueryHostProviders { .. } => "query host providers",
            CommandAction::QueryFleetHealth {} => "query fleet health",
            CommandAction::QueryFleetList {} => "query fleet list",
            CommandAction::QueryCrewList { .. } => "query crew list",
            CommandAction::QueryFleetReplicaSnapshot {} => "query fleet replica snapshot",
            CommandAction::QueryDaemonLogs { .. } => "query daemon logs",
            CommandAction::QueryExplainConvoy { .. } => "explain convoy",
            CommandAction::QueryResourceList { .. } => "query resource list",
            CommandAction::QueryResourceGet { .. } => "query resource get",
            CommandAction::ResourceApply { .. } => "apply resource",
            CommandAction::ResourceManifestResolve { resolution, .. } => match resolution {
                ManifestResolution::Sync => "sync manifest resource",
                ManifestResolution::Adopt => "adopt manifest resource",
            },
            CommandAction::ResourceStatusPatch { .. } => "patch resource status",
            CommandAction::ResourceDelete { .. } => "delete resource",
            CommandAction::ResourceWatch { .. } => "watch resources",
        }
    }
}

/// The structured half of an attach resolution: everything the resolver
/// knows about the binding at the moment it mints the attach command. The
/// CLI stamps this onto its enclosing PM pane (pane → identity, #708) —
/// `<host>/<namespace>/<session>` is the canonical join key the catalog
/// publishes against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct AttachBinding {
    /// Host whose daemon owns the session.
    pub host: crate::HostName,
    pub namespace: String,
    /// Session name. Absent when resolution is delegated cross-host and the
    /// local daemon only knows the target host, not the remote session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convoy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vessel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl AttachBinding {
    pub fn resource_ref(&self) -> Option<crate::ResourceRef> {
        match (&self.convoy, &self.vessel, &self.session) {
            (Some(convoy), Some(vessel), _) => Some(
                crate::ResourceRef::new("flotilla.work/v1", "Convoy", &self.namespace, convoy).subresource(format!("vessels/{vessel}")),
            ),
            (Some(convoy), None, _) => Some(crate::ResourceRef::new("flotilla.work/v1", "Convoy", &self.namespace, convoy)),
            (None, _, Some(session)) => Some(crate::ResourceRef::new("flotilla.work/v1", "TerminalSession", &self.namespace, session)),
            (None, _, None) => None,
        }
    }
}

/// Result returned from command execution, or inter-step data passed between steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandValue {
    Ok,
    ConvoyBriefDelivered {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        displaced: Option<String>,
    },
    ConvoyBriefQueued {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        displaced: Option<String>,
    },
    ConvoyBriefWithdrawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        withdrawn: Option<String>,
    },
    RepoTracked {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_from: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_change: Option<RepositoryIdentityChange>,
    },
    RepoUntracked {
        path: PathBuf,
    },
    Refreshed {
        repos: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        identity_changes: Vec<RepositoryIdentityChange>,
    },
    CheckoutCreated {
        branch: String,
        path: QualifiedPath,
    },
    CheckoutRemoved {
        branch: String,
    },
    TerminalPrepared {
        repo_identity: RepoIdentity,
        target_node_id: crate::NodeId,
        branch: String,
        checkout_path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachable_set_id: Option<AttachableSetId>,
        commands: Vec<ResolvedPaneCommand>,
    },
    PreparedWorkspace(Box<PreparedWorkspace>),
    BranchNameGenerated {
        name: String,
        issue_ids: Vec<(String, String)>,
    },
    CheckoutStatus(Box<CheckoutStatus>),
    Error {
        message: String,
    },
    Cancelled,
    AttachCommandResolved {
        plan: crate::ResolvedAttachPlan,
        /// Structured binding for pane→identity stamping; `None` when the
        /// resolving path cannot describe the target session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AttachBinding>,
    },
    CheckoutPathResolved {
        path: PathBuf,
    },
    RepoProviders(Box<RepoProvidersResponse>),
    HostList(Box<HostListResponse>),
    ProjectList(Box<ProjectListResponse>),
    DispatchQueue(Box<DispatchQueueResponse>),
    HostStatus(Box<HostStatusResponse>),
    HostProviders(Box<HostProvidersResponse>),
    FleetHealth(Box<FleetHealthResponse>),
    FleetList(Box<FleetListResponse>),
    CrewList(Box<CrewListResponse>),
    FleetReplicaSnapshot(Box<FleetReplicaSnapshot>),
    DaemonLogs {
        /// Complete JSON-lines records, oldest first.
        lines: Vec<String>,
    },
    ConvoyExplanation(Box<ConvoyExplanation>),
    ResourceRead(Box<ResourceReadEnvelope>),
    ResourceObject(Box<ResourceJsonResponse>),
    ResourceDeleted(Box<ResourceJsonResponse>),
    ResourceAlreadyDeleted(Box<ResourceJsonResponse>),
    ResourceWatchEvent(Box<ResourceReadEnvelope>),
    EnvironmentSpecRead {
        spec: crate::EnvironmentSpec,
    },
    IssuePage(IssueResultPage),
    IssuesByIds {
        items: Vec<crate::provider_data::Issue>,
    },
    ConvoyCreated {
        name: String,
    },
    ConvoyAbandoned {
        name: String,
        archives: Vec<CheckoutArchiveOutcome>,
    },
    ConvoyStarted {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach_plan: Option<crate::ResolvedAttachPlan>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AttachBinding>,
    },
    WorkflowTemplateApplied {
        name: String,
    },
    ProjectAdded {
        name: String,
    },
    ProjectApplied {
        name: String,
    },
    ProjectRegistered {
        name: String,
        members: usize,
    },
    ProjectRefreshed {
        name: String,
        members: usize,
        converged: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        operational_entries: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryIdentityChange {
    pub previous_display: String,
    pub current_display: String,
}

/// Status of an individual step within a multi-step command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StepStatus {
    Skipped,
    Started,
    Succeeded,
    Produced { value: Box<CommandValue> },
    Failed { message: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutStatus {
    pub branch: String,
    pub change_request_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
    #[serde(default)]
    pub uncommitted_files: Vec<String>,
    pub base_detection_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        arg::Arg,
        query::{
            CrewListMember, CrewListResponse, FleetListResponse, FleetListRow, FleetReplicaSnapshot, FleetReplicaStatus, FleetStaleness,
            HostListEntry, HostListResponse, HostProvidersResponse, HostStatusResponse, RepoProvidersResponse,
        },
        test_helpers::assert_json_roundtrip,
        AttachableSetId, HostEnvironment, HostProviderStatus, HostSummary, NodeId, NodeInfo, PeerConnectionState, RepoIdentity, SystemInfo,
        ToolInventory,
    };

    fn repo_identity() -> RepoIdentity {
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
    }

    #[test]
    fn command_roundtrip_covers_all_variants() {
        let cases = vec![
            Command::builder()
                .action(CommandAction::Refresh { repo: Some(RepoSelector::Query("flotilla".into())) })
                .node_id(NodeId::new("feta"))
                .build(),
            Command::builder().action(CommandAction::TrackRepoPath { path: PathBuf::from("/repo") }).build(),
            Command::builder()
                .action(CommandAction::CreateWorkspaceFromPreparedTerminal {
                    target_node_id: NodeId::new("desktop"),
                    branch: "feat-x".into(),
                    checkout_path: PathBuf::from("/remote/repo/feat-x"),
                    attachable_set_id: Some(AttachableSetId::new("set-1")),
                    commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
                })
                .context_repo(RepoSelector::Path(PathBuf::from("/repo")))
                .build(),
            Command::builder().action(CommandAction::UntrackRepo { repo: RepoSelector::Query("owner/repo".into()) }).build(),
            Command::builder()
                .action(CommandAction::Checkout {
                    repo: RepoSelector::Path(PathBuf::from("/repo")),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![("github".into(), "42".into())],
                })
                .build(),
            Command::builder()
                .action(CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from("/remote/repo/feat-x"), commands: vec![] })
                .node_id(NodeId::new("desktop"))
                .context_repo(RepoSelector::Identity(repo_identity()))
                .build(),
            Command::builder().action(CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("feat-x".into()) }).build(),
            Command::builder()
                .action(CommandAction::FetchCheckoutStatus {
                    branch: "feat-x".into(),
                    checkout_path: None,
                    change_request_id: Some("123".into()),
                })
                .context_repo(RepoSelector::Path(PathBuf::from("/repo")))
                .build(),
            Command::builder()
                .action(CommandAction::CreateWorkspaceForCheckout { checkout_path: PathBuf::from("/repo/wt"), label: "feat-x".into() })
                .context_repo(RepoSelector::Identity(repo_identity()))
                .build(),
            Command::builder().action(CommandAction::SelectWorkspace { ws_ref: "ws://1".into() }).build(),
            Command::builder()
                .action(CommandAction::Attach { reference: "convoy-a".into(), host: None, mode: AttachMode::Default })
                .build(),
            Command::builder()
                .action(CommandAction::AttachTransient {
                    reference: "terminal-scratch".into(),
                    host: Some(crate::HostName::new("feta")),
                    mode: AttachMode::Default,
                })
                .build(),
            Command::builder()
                .action(CommandAction::OpenChangeRequest { id: "99".into() })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::CloseChangeRequest { id: "99".into() })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::MergeChangeRequest { id: "99".into(), confirmed: true })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::OpenIssue { id: "42".into() })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::LinkIssuesToChangeRequest {
                    change_request_id: "99".into(),
                    issue_ids: vec!["42".into(), "43".into()],
                })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::ArchiveSession { session_id: "session-1".into() })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::GenerateBranchName { issue_keys: vec!["ISSUE-1".into(), "ISSUE-2".into()] })
                .context_repo(RepoSelector::Query("owner/repo".into()))
                .build(),
            Command::builder()
                .action(CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-a".into(),
                    work: "implement".into(),
                    message: Some("done".into()),
                })
                .build(),
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some("flotilla".into()), name: "failed-convoy".into(), force: false })
                .build(),
            Command::builder()
                .action(CommandAction::ConvoyResume {
                    namespace: Some("flotilla".into()),
                    name: "convoy-a".into(),
                    prompt: "Rebase onto main".into(),
                    vessel: Some("implement".into()),
                    role: Some("coder".into()),
                })
                .build(),
            Command::builder()
                .action(CommandAction::ConvoyWithdrawPendingBrief { namespace: Some("flotilla".into()), name: "convoy-a".into() })
                .build(),
            Command::builder()
                .action(CommandAction::CrewHandoff {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                    target: "reviewer".into(),
                    message: "review this".into(),
                })
                .build(),
            Command::builder()
                .action(CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                    message: Some("ready for review".into()),
                    disposition: Some("changes-pushed".into()),
                    decision_ledger_ref: Some("https://github.com/flotilla-org/flotilla/pull/1#issuecomment-2".into()),
                })
                .build(),
            Command::builder()
                .action(CommandAction::CrewFail {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                    message: "blocked".into(),
                })
                .build(),
            Command::builder()
                .action(CommandAction::ConvoyCreate {
                    name: "my-convoy".into(),
                    workflow_ref: "scratch".into(),
                    inputs: vec![("topic".into(), "convoy-create-cli".into())],
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".into()),
                    r#ref: Some("main".into()),
                    project_ref: Some("my-project".into()),
                    placement_policy: Some("host-direct-local".into()),
                    adopted_checkout: None,
                })
                .build(),
            Command::builder()
                .action(CommandAction::WorkflowTemplateApply { name: "scratch".into(), spec_yaml: "vessels: []\n".into() })
                .build(),
            Command::builder()
                .action(CommandAction::ProjectAdd {
                    target: "/src/flotilla".into(),
                    name: Some("my-project".into()),
                    display_name: Some("My Project".into()),
                    remote: Some("origin".into()),
                })
                .build(),
            Command::builder()
                .action(CommandAction::ProjectApply { name: "my-project".into(), spec_yaml: "repositories: []\n".into() })
                .build(),
            Command::builder()
                .action(CommandAction::TeleportSession {
                    session_id: "session-1".into(),
                    branch: Some("feat-x".into()),
                    checkout_key: Some(PathBuf::from("/repo/wt")),
                })
                .node_id(NodeId::new("feta"))
                .context_repo(RepoSelector::Identity(repo_identity()))
                .build(),
            Command::builder().action(CommandAction::QueryRepoProviders { repo: RepoSelector::Path(PathBuf::from("/repo")) }).build(),
            Command::builder().action(CommandAction::QueryHostList {}).build(),
            Command::builder().action(CommandAction::QueryProjectList {}).build(),
            Command::builder().action(CommandAction::QueryDispatchQueue { project: Some("widgets".to_string()) }).build(),
            Command::builder().action(CommandAction::QueryFleetList {}).build(),
            Command::builder()
                .action(CommandAction::QueryCrewList {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                })
                .build(),
            Command::builder().action(CommandAction::QueryFleetReplicaSnapshot {}).build(),
            Command::builder()
                .action(CommandAction::QueryDaemonLogs {
                    query: DaemonLogQuery {
                        since_seconds: Some(7200),
                        level: Some("warn".into()),
                        target: Some("flotilla_daemon::peer".into()),
                    },
                })
                .node_id(NodeId::new("feta"))
                .build(),
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".into()), name: "held-work".into() })
                .node_id(NodeId::new("feta"))
                .build(),
            Command::builder()
                .action(CommandAction::QueryResourceList { namespace: "flotilla".into(), kind: "convoys".into(), include_replicas: false })
                .node_id(NodeId::new("feta"))
                .build(),
            Command::builder()
                .action(CommandAction::QueryResourceGet {
                    namespace: "flotilla".into(),
                    kind: "convoys".into(),
                    name: "resource-demo".into(),
                })
                .node_id(NodeId::new("feta"))
                .build(),
            Command::builder()
                .action(CommandAction::ResourceStatusPatch {
                    namespace: "flotilla".into(),
                    kind: "usages".into(),
                    name: "usage-account".into(),
                    status: serde_json::json!({"provider": "codex", "observed_at": "2026-08-08T18:00:00Z"}),
                })
                .build(),
            Command::builder()
                .action(CommandAction::ResourceWatch {
                    namespace: "flotilla".into(),
                    kind: "convoys".into(),
                    name: None,
                    include_replicas: false,
                    replica_sources: false,
                    cursor: None,
                })
                .node_id(NodeId::new("feta"))
                .build(),
            Command::builder()
                .action(CommandAction::QueryHostStatus { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) })
                .build(),
            Command::builder()
                .action(CommandAction::QueryHostProviders { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) })
                .build(),
            Command::builder()
                .action(CommandAction::QueryIssues {
                    repo: RepoSelector::Query("test".into()),
                    params: crate::issue_query::IssueQuery::default(),
                    page: 1,
                    count: 50,
                })
                .build(),
            Command::builder()
                .action(CommandAction::QueryIssueFetchByIds {
                    repo: RepoSelector::Path(PathBuf::from("/repo")),
                    ids: vec!["1".into(), "2".into()],
                })
                .build(),
            Command::builder()
                .action(CommandAction::QueryIssueOpenInBrowser { repo: RepoSelector::Path(PathBuf::from("/repo")), id: "42".into() })
                .build(),
        ];

        for cmd in cases {
            assert_json_roundtrip(&cmd);
        }
    }

    #[test]
    fn command_uses_snake_case_tag() {
        let cmd = Command::builder().action(CommandAction::SelectWorkspace { ws_ref: "x".into() }).build();
        let json = serde_json::to_value(&cmd).expect("serialize");
        assert_eq!(json.get("action").and_then(|v| v.as_str()), Some("select_workspace"));
    }

    #[test]
    fn command_value_roundtrip_covers_all_variants() {
        let cases = vec![
            CommandValue::Ok,
            CommandValue::ConvoyBriefDelivered { displaced: Some("older instruction".into()) },
            CommandValue::ConvoyBriefQueued { displaced: Some("older instruction".into()) },
            CommandValue::ConvoyBriefWithdrawn { withdrawn: Some("latest instruction".into()) },
            CommandValue::RepoTracked {
                path: PathBuf::from("/new/repo"),
                resolved_from: None,
                identity_change: Some(RepositoryIdentityChange {
                    previous_display: "local".to_string(),
                    current_display: "https://github.com/flotilla-org/flotilla".to_string(),
                }),
            },
            CommandValue::RepoUntracked { path: PathBuf::from("/old/repo") },
            CommandValue::Refreshed { repos: vec![PathBuf::from("/repo-a"), PathBuf::from("/repo-b")], identity_changes: Vec::new() },
            CommandValue::CheckoutCreated {
                branch: "feat-new".into(),
                path: QualifiedPath::host(HostId::new("host-a"), "/repos/project/wt-1"),
            },
            CommandValue::CheckoutRemoved { branch: "feat-old".into() },
            CommandValue::TerminalPrepared {
                repo_identity: repo_identity(),
                target_node_id: NodeId::new("desktop"),
                branch: "feat-x".into(),
                checkout_path: PathBuf::from("/remote/repo/feat-x"),
                attachable_set_id: Some(AttachableSetId::new("set-1")),
                commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
            },
            CommandValue::PreparedWorkspace(Box::new(PreparedWorkspace {
                label: "feat-x".into(),
                target_node_id: NodeId::new("desktop"),
                display_host: Some(crate::HostName::new("desktop")),
                checkout_path: PathBuf::from("/remote/repo/feat-x"),
                checkout_key: None,
                attachable_set_id: Some(AttachableSetId::new("set-1")),
                environment_id: None,
                container_name: None,
                template_yaml: Some("layout: []\ncontent: []\n".into()),
                prepared_commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
            })),
            CommandValue::BranchNameGenerated { name: "feat/cool-thing".into(), issue_ids: vec![("gh".into(), "1".into())] },
            CommandValue::CheckoutStatus(Box::new(CheckoutStatus {
                branch: "old".into(),
                change_request_status: Some("merged".into()),
                merge_commit_sha: Some("abc123".into()),
                unpushed_commits: vec!["def456".into()],
                has_uncommitted: true,
                uncommitted_files: vec!["M  src/main.rs".into(), "?? TODO.txt".into()],
                base_detection_warning: Some("warning text".into()),
            })),
            CommandValue::Error { message: "something failed".into() },
            CommandValue::Cancelled,
            CommandValue::AttachCommandResolved { plan: crate::ResolvedAttachPlan::shell_command("bash --login"), binding: None },
            CommandValue::CheckoutPathResolved { path: PathBuf::from("/repos/project/wt-1") },
            CommandValue::RepoProviders(Box::new(RepoProvidersResponse {
                path: PathBuf::from("/repo"),
                slug: Some("owner/repo".into()),
                host_discovery: vec![],
                repo_discovery: vec![],
                providers: vec![],
                unmet_requirements: vec![],
            })),
            CommandValue::HostList(Box::new(HostListResponse {
                hosts: vec![HostListEntry {
                    environment_id: Some(EnvironmentId::host(HostId::new("desktop-host"))),
                    host_name: crate::HostName::new("desktop"),
                    node: Some(NodeInfo::new(NodeId::new("desktop"), "Desktop")),
                    is_local: true,
                    configured: true,
                    connection_status: PeerConnectionState::Connected,
                    reconnect: None,
                    has_summary: true,
                    repo_count: 1,
                }],
            })),
            CommandValue::ProjectList(Box::new(crate::ProjectListResponse { projects: vec![] })),
            CommandValue::HostStatus(Box::new(HostStatusResponse {
                environment_id: EnvironmentId::host(HostId::new("desktop-host")),
                host_name: crate::HostName::new("desktop"),
                node: NodeInfo::new(NodeId::new("desktop"), "Desktop"),
                is_local: true,
                configured: true,
                connection_status: PeerConnectionState::Connected,
                summary: Some(HostSummary {
                    environment_id: EnvironmentId::host(HostId::new("desktop-host")),
                    host_name: Some(crate::HostName::new("desktop")),
                    node: NodeInfo::new(NodeId::new("desktop"), "Desktop"),
                    system: SystemInfo {
                        home_dir: Some("/home/dev".into()),
                        os: Some("linux".into()),
                        arch: Some("aarch64".into()),
                        cpu_count: Some(8),
                        memory_total_mb: Some(16384),
                        environment: HostEnvironment::Unknown,
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
                }),
                visible_environments: vec![],
                repo_count: 1,
            })),
            CommandValue::HostProviders(Box::new(HostProvidersResponse {
                environment_id: EnvironmentId::host(HostId::new("desktop-host")),
                host_name: crate::HostName::new("desktop"),
                node: NodeInfo::new(NodeId::new("desktop"), "Desktop"),
                is_local: true,
                configured: true,
                connection_status: PeerConnectionState::Connected,
                summary: HostSummary {
                    environment_id: EnvironmentId::host(HostId::new("desktop-host")),
                    host_name: Some(crate::HostName::new("desktop")),
                    node: NodeInfo::new(NodeId::new("desktop"), "Desktop"),
                    system: SystemInfo::default(),
                    inventory: ToolInventory::default(),
                    providers: vec![],
                    environments: vec![],
                },
                visible_environments: vec![],
            })),
            CommandValue::FleetList(Box::new(FleetListResponse {
                rows: vec![FleetListRow::builder()
                    .convoy("convoy-a")
                    .vessel("vessel-a")
                    .authority("adopted")
                    .crew("implement/main")
                    .crew_state("running")
                    .host(crate::HostName::new("desktop"))
                    .namespace("dev")
                    .staleness(FleetStaleness::Local)
                    .build()],
                replicas: vec![FleetReplicaStatus {
                    host: crate::HostName::new("feta"),
                    reachable: false,
                    last_sync: None,
                    generation: None,
                    skipped_records: 0,
                    first_parse_error: None,
                    message: Some("not synced".into()),
                }],
            })),
            CommandValue::CrewList(Box::new(CrewListResponse {
                convoy: "convoy-a".into(),
                vessel_ref: "convoy-a-implement".into(),
                vessel: "implement".into(),
                members: vec![CrewListMember {
                    role: "coder".into(),
                    kind: "agent".into(),
                    state: "active".into(),
                    attention: None,
                    adapter: Some("codex".into()),
                    model: None,
                    stance: Some("trusted-implicit".into()),
                }],
            })),
            CommandValue::FleetReplicaSnapshot(Box::new(FleetReplicaSnapshot {
                host: crate::HostName::new("desktop"),
                generation: Some("7".into()),
                rows: vec![FleetListRow::builder()
                    .convoy("convoy-a")
                    .vessel("vessel-a")
                    .crew("main")
                    .crew_state("exited")
                    .host(crate::HostName::new("desktop"))
                    .namespace("dev")
                    .staleness(FleetStaleness::Local)
                    .build()],
                result_sets: vec![],
            })),
            CommandValue::ResourceRead(Box::new(ResourceReadEnvelope {
                api_version: "flotilla.work/v1".into(),
                resource_kind: "Convoy".into(),
                plural: "convoys".into(),
                namespace: "flotilla".into(),
                cursor: ResourceCursor::from_position("1", None),
                records: vec![ResourceReadRecord {
                    record_type: ResourceRecordType::Current,
                    provenance: ResourceRecordProvenance::Local { node_id: crate::NodeId::new("desktop") },
                    object: Some(serde_json::json!({
                        "apiVersion": "flotilla.work/v1",
                        "kind": "Convoy",
                        "metadata": { "name": "demo" },
                        "spec": {}
                    })),
                }],
            })),
            CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                kind: "Convoy".into(),
                plural: "convoys".into(),
                namespace: "flotilla".into(),
                value: serde_json::json!({
                    "apiVersion": "flotilla.work/v1",
                    "kind": "Convoy",
                    "metadata": { "name": "demo" },
                    "spec": {}
                }),
                replica_origin: None,
            })),
            CommandValue::ResourceWatchEvent(Box::new(ResourceReadEnvelope {
                api_version: "flotilla.work/v1".into(),
                resource_kind: "Convoy".into(),
                plural: "convoys".into(),
                namespace: "flotilla".into(),
                cursor: ResourceCursor::from_position("7", None),
                records: vec![ResourceReadRecord {
                    record_type: ResourceRecordType::Added,
                    provenance: ResourceRecordProvenance::Local { node_id: crate::NodeId::new("desktop") },
                    object: Some(serde_json::json!({
                        "apiVersion": "flotilla.work/v1",
                        "kind": "Convoy",
                        "metadata": { "name": "demo" },
                        "spec": {}
                    })),
                }],
            })),
            CommandValue::EnvironmentSpecRead {
                spec: crate::EnvironmentSpec {
                    image: crate::ImageSource::Registry("ubuntu:24.04".into()),
                    token_env_vars: vec!["GITHUB_TOKEN".into()],
                },
            },
            CommandValue::IssuePage(crate::issue_query::IssueResultPage { items: vec![], total: Some(10), has_more: true }),
            CommandValue::IssuesByIds { items: vec![] },
            CommandValue::ConvoyCreated { name: "my-convoy".into() },
            CommandValue::ConvoyAbandoned {
                name: "my-convoy".into(),
                archives: vec![CheckoutArchiveOutcome::builder()
                    .checkout("work".to_string())
                    .status(CheckoutArchiveStatus::NothingToArchive)
                    .build()],
            },
            CommandValue::WorkflowTemplateApplied { name: "scratch".into() },
            CommandValue::ProjectAdded { name: "my-project".into() },
            CommandValue::ProjectApplied { name: "my-project".into() },
        ];

        for result in cases {
            assert_json_roundtrip(&result);
        }
    }

    #[test]
    fn prepared_workspace_roundtrip_preserves_fields() {
        let prepared = PreparedWorkspace {
            label: "feat-x".into(),
            target_node_id: NodeId::new("desktop"),
            display_host: Some(crate::HostName::new("desktop")),
            checkout_path: PathBuf::from("/remote/repo/feat-x"),
            checkout_key: None,
            attachable_set_id: Some(AttachableSetId::new("set-1")),
            environment_id: None,
            container_name: None,
            template_yaml: Some("layout: []\ncontent: []\n".into()),
            prepared_commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
        };

        assert_json_roundtrip(&prepared);
    }

    #[test]
    fn command_result_uses_snake_case_tag() {
        let result = CommandValue::CheckoutCreated { branch: "x".into(), path: QualifiedPath::host(HostId::new("host-a"), "/tmp/x") };
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(json.get("kind").and_then(|v| v.as_str()), Some("checkout_created"));
    }

    #[test]
    fn repo_selector_identity_roundtrip() {
        assert_json_roundtrip(&RepoSelector::Identity(repo_identity()));
    }

    #[test]
    fn issue_selector_json_is_stable_and_roundtrips_all_variants() {
        let cases = [
            (IssueSelector::Id("834".into()), json!({"kind": "id", "value": "834"})),
            (
                IssueSelector::Alias { alias: "zellij".into(), id: "12".into() },
                json!({"kind": "alias", "value": {"alias": "zellij", "id": "12"}}),
            ),
            (
                IssueSelector::Reference(IssueRef {
                    source: crate::IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
                    id: "834".into(),
                }),
                json!({
                    "kind": "reference",
                    "value": {
                        "source": {"service": "https://github.com", "scope": "flotilla-org/flotilla"},
                        "id": "834"
                    }
                }),
            ),
        ];

        for (selector, expected_json) in cases {
            assert_eq!(serde_json::to_value(&selector).expect("serialize"), expected_json);
            assert_json_roundtrip(&selector);
        }
    }

    #[test]
    fn step_status_roundtrip() {
        use crate::test_helpers::assert_roundtrip;

        let cases = vec![
            StepStatus::Skipped,
            StepStatus::Started,
            StepStatus::Succeeded,
            StepStatus::Produced { value: Box::new(CommandValue::Ok) },
            StepStatus::Failed { message: "workspace creation failed".into() },
        ];
        for case in cases {
            assert_roundtrip(&case);
        }
    }

    #[test]
    fn checkout_status_default() {
        let info = CheckoutStatus::default();
        assert_eq!(info.branch, "");
        assert!(info.change_request_status.is_none());
        assert!(info.merge_commit_sha.is_none());
        assert!(info.unpushed_commits.is_empty());
        assert!(!info.has_uncommitted);
        assert!(info.uncommitted_files.is_empty());
        assert!(info.base_detection_warning.is_none());
    }

    #[test]
    fn checkout_status_roundtrip_preserves_fields() {
        let info = CheckoutStatus {
            branch: "old-feat".into(),
            change_request_status: Some("closed".into()),
            merge_commit_sha: Some("deadbeef".into()),
            unpushed_commits: vec!["aaa".into(), "bbb".into()],
            has_uncommitted: true,
            uncommitted_files: vec!["M  src/lib.rs".into()],
            base_detection_warning: Some("ambiguous base".into()),
        };
        assert_json_roundtrip(&info);
    }

    #[test]
    fn command_description_covers_all_variants() {
        let cases: Vec<Command> = vec![
            Command::builder()
                .action(CommandAction::CreateWorkspaceForCheckout { checkout_path: PathBuf::from("/tmp"), label: "ws".into() })
                .build(),
            Command::builder()
                .action(CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from("/remote/repo/feat-x"), commands: vec![] })
                .node_id(NodeId::new("desktop"))
                .context_repo(RepoSelector::Identity(repo_identity()))
                .build(),
            Command::builder()
                .action(CommandAction::CreateWorkspaceFromPreparedTerminal {
                    target_node_id: NodeId::new("desktop"),
                    branch: "feat-x".into(),
                    checkout_path: PathBuf::from("/remote/repo/feat-x"),
                    attachable_set_id: None,
                    commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
                })
                .context_repo(RepoSelector::Identity(repo_identity()))
                .build(),
            Command::builder().action(CommandAction::SelectWorkspace { ws_ref: "x".into() }).build(),
            Command::builder()
                .action(CommandAction::Checkout {
                    repo: RepoSelector::Query("repo".into()),
                    target: CheckoutTarget::Branch("b".into()),
                    issue_ids: vec![],
                })
                .build(),
            Command::builder().action(CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("b".into()) }).build(),
            Command::builder()
                .action(CommandAction::FetchCheckoutStatus { branch: "b".into(), checkout_path: None, change_request_id: None })
                .build(),
            Command::builder()
                .action(CommandAction::OpenChangeRequest { id: "1".into() })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::CloseChangeRequest { id: "1".into() })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::MergeChangeRequest { id: "1".into(), confirmed: true })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::OpenIssue { id: "1".into() })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::LinkIssuesToChangeRequest { change_request_id: "1".into(), issue_ids: vec![] })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::ArchiveSession { session_id: "s".into() })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::GenerateBranchName { issue_keys: vec![] })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder()
                .action(CommandAction::ConvoyDelete { namespace: Some("flotilla".into()), name: "failed-convoy".into(), force: false })
                .build(),
            Command::builder()
                .action(CommandAction::TeleportSession { session_id: "s".into(), branch: None, checkout_key: None })
                .context_repo(RepoSelector::Path(PathBuf::from("/tmp")))
                .build(),
            Command::builder().action(CommandAction::TrackRepoPath { path: PathBuf::from("/tmp") }).build(),
            Command::builder().action(CommandAction::UntrackRepo { repo: RepoSelector::Path(PathBuf::from("/tmp")) }).build(),
            Command::builder().action(CommandAction::Refresh { repo: None }).build(),
            Command::builder().action(CommandAction::QueryRepoProviders { repo: RepoSelector::Path(PathBuf::from("/tmp")) }).build(),
            Command::builder().action(CommandAction::QueryHostList {}).build(),
            Command::builder().action(CommandAction::QueryProjectList {}).build(),
            Command::builder()
                .action(CommandAction::QueryHostStatus { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) })
                .build(),
            Command::builder()
                .action(CommandAction::QueryHostProviders { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) })
                .build(),
            Command::builder()
                .action(CommandAction::QueryIssues {
                    repo: RepoSelector::Query("test".into()),
                    params: crate::issue_query::IssueQuery::default(),
                    page: 1,
                    count: 50,
                })
                .build(),
            Command::builder()
                .action(CommandAction::QueryIssueFetchByIds { repo: RepoSelector::Path(PathBuf::from("/repo")), ids: vec!["1".into()] })
                .build(),
            Command::builder()
                .action(CommandAction::QueryIssueOpenInBrowser { repo: RepoSelector::Path(PathBuf::from("/repo")), id: "42".into() })
                .build(),
        ];
        for cmd in cases {
            let desc = cmd.description();
            assert!(!desc.is_empty(), "empty description for {:?}", cmd);
        }
    }

    #[test]
    fn query_issues_roundtrip() {
        let cmd = CommandAction::QueryIssues {
            repo: RepoSelector::Query("test".into()),
            params: crate::issue_query::IssueQuery::default(),
            page: 1,
            count: 50,
        };
        assert_json_roundtrip(&cmd);
    }

    #[test]
    fn issue_page_value_roundtrip() {
        let val = CommandValue::IssuePage(crate::issue_query::IssueResultPage { items: vec![], total: Some(10), has_more: true });
        assert_json_roundtrip(&val);
    }

    #[test]
    fn resource_cursor_is_opaque_and_roundtrips_its_position() {
        let cursor = ResourceCursor::from_position("42", Some("observed-generation".to_string()));

        assert!(!cursor.to_string().contains("42"));
        assert_eq!(cursor.position().expect("decode cursor"), ("42".to_string(), Some("observed-generation".to_string())));
        assert_eq!(cursor.to_string().parse::<ResourceCursor>().expect("parse cursor"), cursor);
    }

    #[test]
    fn resource_read_envelope_uses_the_stable_script_shape() {
        let envelope = ResourceReadEnvelope {
            api_version: "flotilla.work/v1".into(),
            resource_kind: "Convoy".into(),
            plural: "convoys".into(),
            namespace: "flotilla".into(),
            cursor: ResourceCursor::from_position("1", None),
            records: vec![ResourceReadRecord {
                record_type: ResourceRecordType::Current,
                provenance: ResourceRecordProvenance::Local { node_id: crate::NodeId::new("feta") },
                object: Some(serde_json::json!({"metadata": {"name": "demo"}})),
            }],
        };

        let value = serde_json::to_value(&envelope).expect("serialize resource envelope");
        assert_eq!(value["apiVersion"], "flotilla.work/v1");
        assert_eq!(value["resourceKind"], "Convoy");
        assert!(value["cursor"].is_string());
        assert_eq!(value["records"][0]["type"], "CURRENT");
        assert_eq!(value["records"][0]["provenance"]["source"], "local");
        assert_json_roundtrip(&envelope);
    }

    #[test]
    fn only_no_attach_suppresses_the_dispatch_regard() {
        assert_eq!(ConvoyDispatchRegard::from(ConvoyAutoAttach::Never), ConvoyDispatchRegard::Suppress);
        assert_eq!(ConvoyDispatchRegard::from(ConvoyAutoAttach::Default), ConvoyDispatchRegard::Emit);
        assert_eq!(ConvoyDispatchRegard::from(ConvoyAutoAttach::Always), ConvoyDispatchRegard::Emit);
    }

    #[test]
    fn dispatch_queue_command_value_has_a_stable_json_shape() {
        let ready_at = "2026-08-04T12:00:00Z".parse().expect("timestamp");
        let value = CommandValue::DispatchQueue(Box::new(crate::DispatchQueueResponse {
            observed_at: ready_at,
            entries: vec![crate::DispatchQueueRow::builder()
                .namespace("flotilla".to_string())
                .project("widgets".to_string())
                .issue(crate::IssueRef {
                    source: crate::IssueSource { service: "https://github.com".to_string(), scope: "acme/widgets".to_string() },
                    id: "42".to_string(),
                })
                .title("Fix the queue".to_string())
                .ready_observed_at(ready_at)
                .age_seconds(90)
                .attention(true)
                .provenance("dispatch-reconciler".to_string())
                .build()],
        }));

        assert_eq!(
            serde_json::to_value(value).expect("serialize"),
            json!({
                "kind": "dispatch_queue",
                "observed_at": "2026-08-04T12:00:00Z",
                "entries": [{
                    "namespace": "flotilla",
                    "project": "widgets",
                    "issue": {
                        "source": {"service": "https://github.com", "scope": "acme/widgets"},
                        "id": "42"
                    },
                    "title": "Fix the queue",
                    "ready_observed_at": "2026-08-04T12:00:00Z",
                    "age_seconds": 90,
                    "attention": true,
                    "provenance": "dispatch-reconciler"
                }]
            })
        );
    }
}
