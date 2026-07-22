use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    arg::Arg,
    issue_query::{IssueQuery, IssueResultPage},
    qualified_path::QualifiedPath,
    query::{
        CrewCommandContext, CrewListResponse, FleetListResponse, FleetReplicaSnapshot, HostListResponse, HostProvidersResponse,
        HostStatusResponse, ProjectListResponse, RepoDetailResponse, RepoProvidersResponse, RepoWorkResponse,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ResourceWatchResponse {
    #[serde(rename = "resourceKind")]
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub event: serde_json::Value,
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

/// An issue supplied to convoy admission either as a fully source-qualified
/// reference or as an opaque ID whose source must be resolved through the
/// Project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum IssueSelector {
    Id(String),
    Reference(IssueRef),
}

/// Partial intent accepted by the convoy start verb. Admission completes any
/// omitted fields before persisting a `ConvoySpec`.
///
/// This type lives in protocol rather than resources because incomplete
/// intent must never enter the resource store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyStartIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub project_ref: String,
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
    #[builder(default)]
    #[serde(default)]
    pub auto_attach: bool,
}

/// A convoy launch admitted by the presentation host and ready to be
/// persisted by the selected execution host.
///
/// Resource specs remain opaque at the protocol boundary so the protocol
/// crate does not depend on the resource model. The execution host validates
/// and deserializes each snapshot before storing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PreparedConvoyStart {
    pub namespace: String,
    pub name: String,
    pub convoy_spec: serde_json::Value,
    pub workflow_name: String,
    pub workflow_spec: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy_spec: Option<serde_json::Value>,
    #[builder(default)]
    #[serde(default)]
    pub auto_attach: bool,
}

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
    },
    /// Resolve an attach for a temporary foreground excursion. Unlike the
    /// human-facing CLI attach, recursive hops must not stamp PM metadata.
    AttachTransient {
        reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<crate::HostName>,
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
    CrewHandoff {
        context: CrewCommandContext,
        target: String,
        message: String,
    },
    CrewComplete {
        context: CrewCommandContext,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
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
    /// Internal peer command carrying the exact resource snapshots admitted
    /// by the presentation host.
    ConvoyStartPrepared {
        start: Box<PreparedConvoyStart>,
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
    QueryRepoDetail {
        repo: RepoSelector,
    },
    QueryRepoProviders {
        repo: RepoSelector,
    },
    QueryRepoWork {
        repo: RepoSelector,
    },
    QueryHostList {},
    QueryProjectList {},
    QueryHostStatus {
        target_environment_id: crate::EnvironmentId,
    },
    QueryHostProviders {
        target_environment_id: crate::EnvironmentId,
    },
    QueryFleetList {},
    QueryCrewList {
        context: CrewCommandContext,
    },
    QueryFleetReplicaSnapshot {},
    QueryResourceList {
        namespace: String,
        kind: String,
    },
    QueryResourceGet {
        namespace: String,
        kind: String,
        name: String,
    },
    ResourceWatch {
        namespace: String,
        kind: String,
    },
}

impl CommandAction {
    /// Whether this action is a read-only query command.
    pub fn is_query(&self) -> bool {
        matches!(
            self,
            CommandAction::QueryRepoDetail { .. }
                | CommandAction::QueryRepoProviders { .. }
                | CommandAction::QueryRepoWork { .. }
                | CommandAction::QueryHostList {}
                | CommandAction::QueryProjectList {}
                | CommandAction::QueryHostStatus { .. }
                | CommandAction::QueryHostProviders { .. }
                | CommandAction::QueryFleetList {}
                | CommandAction::QueryCrewList { .. }
                | CommandAction::QueryFleetReplicaSnapshot {}
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
            CommandAction::OpenIssue { .. } => "Opening in browser...",
            CommandAction::LinkIssuesToChangeRequest { .. } => "Linking issues...",
            CommandAction::ArchiveSession { .. } => "Archiving session...",
            CommandAction::GenerateBranchName { .. } => "Generating branch name...",
            CommandAction::ConvoyWorkForceComplete { .. } => "Force-completing work...",
            CommandAction::ConvoyDelete { .. } => "Deleting convoy...",
            CommandAction::ConvoyAbandon { .. } => "Abandoning convoy...",
            CommandAction::CrewHandoff { .. } => "Handing off to crew member...",
            CommandAction::CrewComplete { .. } => "Completing crew work...",
            CommandAction::CrewFail { .. } => "Failing crew work...",
            CommandAction::ConvoyCreate { .. } => "Creating convoy...",
            CommandAction::ConvoyStart { .. } => "Starting convoy...",
            CommandAction::ConvoyStartPrepared { .. } => "Starting convoy...",
            CommandAction::WorkflowTemplateApply { .. } => "Applying workflow template...",
            CommandAction::ProjectAdd { .. } => "Adding project...",
            CommandAction::ProjectApply { .. } => "Applying project...",
            CommandAction::TeleportSession { .. } => "Teleporting session...",
            CommandAction::TrackRepoPath { .. } => "Tracking repository...",
            CommandAction::UntrackRepo { .. } => "Untracking repository...",
            CommandAction::Refresh { .. } => "Refreshing...",
            CommandAction::QueryIssues { .. } => "query issues",
            CommandAction::QueryIssueFetchByIds { .. } => "query issue fetch by ids",
            CommandAction::QueryIssueOpenInBrowser { .. } => "query issue open in browser",
            CommandAction::QueryRepoDetail { .. } => "query repo detail",
            CommandAction::QueryRepoProviders { .. } => "query repo providers",
            CommandAction::QueryRepoWork { .. } => "query repo work",
            CommandAction::QueryHostList {} => "query host list",
            CommandAction::QueryProjectList {} => "query project list",
            CommandAction::QueryHostStatus { .. } => "query host status",
            CommandAction::QueryHostProviders { .. } => "query host providers",
            CommandAction::QueryFleetList {} => "query fleet list",
            CommandAction::QueryCrewList { .. } => "query crew list",
            CommandAction::QueryFleetReplicaSnapshot {} => "query fleet replica snapshot",
            CommandAction::QueryResourceList { .. } => "query resource list",
            CommandAction::QueryResourceGet { .. } => "query resource get",
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

/// Result returned from command execution, or inter-step data passed between steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandValue {
    Ok,
    RepoTracked {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_from: Option<PathBuf>,
    },
    RepoUntracked {
        path: PathBuf,
    },
    Refreshed {
        repos: Vec<PathBuf>,
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
    PreparedWorkspace(PreparedWorkspace),
    BranchNameGenerated {
        name: String,
        issue_ids: Vec<(String, String)>,
    },
    CheckoutStatus(CheckoutStatus),
    Error {
        message: String,
    },
    Cancelled,
    AttachCommandResolved {
        command: String,
        /// Structured binding for pane→identity stamping; `None` when the
        /// resolving path cannot describe the target session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AttachBinding>,
    },
    CheckoutPathResolved {
        path: PathBuf,
    },
    RepoDetail(Box<RepoDetailResponse>),
    RepoProviders(Box<RepoProvidersResponse>),
    RepoWork(Box<RepoWorkResponse>),
    HostList(Box<HostListResponse>),
    ProjectList(Box<ProjectListResponse>),
    HostStatus(Box<HostStatusResponse>),
    HostProviders(Box<HostProvidersResponse>),
    FleetList(Box<FleetListResponse>),
    CrewList(Box<CrewListResponse>),
    FleetReplicaSnapshot(Box<FleetReplicaSnapshot>),
    ResourceList(Box<ResourceJsonResponse>),
    ResourceObject(Box<ResourceJsonResponse>),
    ResourceWatchEvent(Box<ResourceWatchResponse>),
    ImageEnsured {
        image: crate::ImageId,
    },
    EnvironmentCreated {
        env_id: crate::EnvironmentId,
    },
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
    ConvoyStarted {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach_command: Option<String>,
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
            HostListEntry, HostListResponse, HostProvidersResponse, HostStatusResponse, RepoDetailResponse, RepoProvidersResponse,
            RepoWorkResponse,
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
            Command {
                node_id: Some(NodeId::new("feta")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: Some(RepoSelector::Query("flotilla".into())) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::TrackRepoPath { path: PathBuf::from("/repo") },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/repo"))),
                action: CommandAction::CreateWorkspaceFromPreparedTerminal {
                    target_node_id: NodeId::new("desktop"),
                    branch: "feat-x".into(),
                    checkout_path: PathBuf::from("/remote/repo/feat-x"),
                    attachable_set_id: Some(AttachableSetId::new("set-1")),
                    commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::UntrackRepo { repo: RepoSelector::Query("owner/repo".into()) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Path(PathBuf::from("/repo")),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![("github".into(), "42".into())],
                },
            },
            Command {
                node_id: Some(NodeId::new("desktop")),
                provisioning_target: None,
                context_repo: Some(RepoSelector::Identity(repo_identity())),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from("/remote/repo/feat-x"), commands: vec![] },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("feat-x".into()) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/repo"))),
                action: CommandAction::FetchCheckoutStatus {
                    branch: "feat-x".into(),
                    checkout_path: None,
                    change_request_id: Some("123".into()),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Identity(repo_identity())),
                action: CommandAction::CreateWorkspaceForCheckout { checkout_path: PathBuf::from("/repo/wt"), label: "feat-x".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::SelectWorkspace { ws_ref: "ws://1".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Attach { reference: "convoy-a".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::AttachTransient { reference: "terminal-scratch".into(), host: Some(crate::HostName::new("feta")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Query("owner/repo".into())),
                action: CommandAction::OpenChangeRequest { id: "99".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Query("owner/repo".into())),
                action: CommandAction::CloseChangeRequest { id: "99".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Query("owner/repo".into())),
                action: CommandAction::OpenIssue { id: "42".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Query("owner/repo".into())),
                action: CommandAction::LinkIssuesToChangeRequest {
                    change_request_id: "99".into(),
                    issue_ids: vec!["42".into(), "43".into()],
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Query("owner/repo".into())),
                action: CommandAction::ArchiveSession { session_id: "session-1".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Query("owner/repo".into())),
                action: CommandAction::GenerateBranchName { issue_keys: vec!["ISSUE-1".into(), "ISSUE-2".into()] },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-a".into(),
                    work: "implement".into(),
                    message: Some("done".into()),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyDelete { namespace: Some("flotilla".into()), name: "failed-convoy".into(), force: false },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewHandoff {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                    target: "reviewer".into(),
                    message: "review this".into(),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                    message: Some("ready for review".into()),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewFail {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                    message: "blocked".into(),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "my-convoy".into(),
                    workflow_ref: "scratch".into(),
                    inputs: vec![("topic".into(), "convoy-create-cli".into())],
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".into()),
                    r#ref: Some("main".into()),
                    project_ref: Some("my-project".into()),
                    placement_policy: Some("host-direct-local".into()),
                    adopted_checkout: None,
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::WorkflowTemplateApply { name: "scratch".into(), spec_yaml: "vessels: []\n".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ProjectAdd {
                    target: "/src/flotilla".into(),
                    name: Some("my-project".into()),
                    display_name: Some("My Project".into()),
                    remote: Some("origin".into()),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ProjectApply { name: "my-project".into(), spec_yaml: "repositories: []\n".into() },
            },
            Command {
                node_id: Some(NodeId::new("feta")),
                provisioning_target: None,
                context_repo: Some(RepoSelector::Identity(repo_identity())),
                action: CommandAction::TeleportSession {
                    session_id: "session-1".into(),
                    branch: Some("feat-x".into()),
                    checkout_key: Some(PathBuf::from("/repo/wt")),
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoDetail { repo: RepoSelector::Path(PathBuf::from("/repo")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoProviders { repo: RepoSelector::Path(PathBuf::from("/repo")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoWork { repo: RepoSelector::Path(PathBuf::from("/repo")) },
            },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryProjectList {} },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetList {} },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryCrewList {
                    context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                },
            },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryFleetReplicaSnapshot {} },
            Command {
                node_id: Some(NodeId::new("feta")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryResourceList { namespace: "flotilla".into(), kind: "convoys".into() },
            },
            Command {
                node_id: Some(NodeId::new("feta")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryResourceGet {
                    namespace: "flotilla".into(),
                    kind: "convoys".into(),
                    name: "resource-demo".into(),
                },
            },
            Command {
                node_id: Some(NodeId::new("feta")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ResourceWatch { namespace: "flotilla".into(), kind: "convoys".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostProviders { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssues {
                    repo: RepoSelector::Query("test".into()),
                    params: crate::issue_query::IssueQuery::default(),
                    page: 1,
                    count: 50,
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueFetchByIds {
                    repo: RepoSelector::Path(PathBuf::from("/repo")),
                    ids: vec!["1".into(), "2".into()],
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueOpenInBrowser { repo: RepoSelector::Path(PathBuf::from("/repo")), id: "42".into() },
            },
        ];

        for cmd in cases {
            assert_json_roundtrip(&cmd);
        }
    }

    #[test]
    fn command_uses_snake_case_tag() {
        let cmd = Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::SelectWorkspace { ws_ref: "x".into() },
        };
        let json = serde_json::to_value(&cmd).expect("serialize");
        assert_eq!(json.get("action").and_then(|v| v.as_str()), Some("select_workspace"));
    }

    #[test]
    fn command_value_roundtrip_covers_all_variants() {
        let cases = vec![
            CommandValue::Ok,
            CommandValue::RepoTracked { path: PathBuf::from("/new/repo"), resolved_from: None },
            CommandValue::RepoUntracked { path: PathBuf::from("/old/repo") },
            CommandValue::Refreshed { repos: vec![PathBuf::from("/repo-a"), PathBuf::from("/repo-b")] },
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
            CommandValue::PreparedWorkspace(PreparedWorkspace {
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
            }),
            CommandValue::BranchNameGenerated { name: "feat/cool-thing".into(), issue_ids: vec![("gh".into(), "1".into())] },
            CommandValue::CheckoutStatus(CheckoutStatus {
                branch: "old".into(),
                change_request_status: Some("merged".into()),
                merge_commit_sha: Some("abc123".into()),
                unpushed_commits: vec!["def456".into()],
                has_uncommitted: true,
                uncommitted_files: vec!["M  src/main.rs".into(), "?? TODO.txt".into()],
                base_detection_warning: Some("warning text".into()),
            }),
            CommandValue::Error { message: "something failed".into() },
            CommandValue::Cancelled,
            CommandValue::AttachCommandResolved { command: "bash --login".into(), binding: None },
            CommandValue::CheckoutPathResolved { path: PathBuf::from("/repos/project/wt-1") },
            CommandValue::RepoDetail(Box::new(RepoDetailResponse {
                path: PathBuf::from("/repo"),
                slug: Some("owner/repo".into()),
                provider_health: Default::default(),
                work_items: vec![],
                errors: vec![],
            })),
            CommandValue::RepoProviders(Box::new(RepoProvidersResponse {
                path: PathBuf::from("/repo"),
                slug: Some("owner/repo".into()),
                host_discovery: vec![],
                repo_discovery: vec![],
                providers: vec![],
                unmet_requirements: vec![],
            })),
            CommandValue::RepoWork(Box::new(RepoWorkResponse {
                path: PathBuf::from("/repo"),
                slug: Some("owner/repo".into()),
                work_items: vec![],
            })),
            CommandValue::HostList(Box::new(HostListResponse {
                hosts: vec![HostListEntry {
                    environment_id: EnvironmentId::host(HostId::new("desktop-host")),
                    host_name: crate::HostName::new("desktop"),
                    node: NodeInfo::new(NodeId::new("desktop"), "Desktop"),
                    is_local: true,
                    configured: true,
                    connection_status: PeerConnectionState::Connected,
                    has_summary: true,
                    repo_count: 1,
                    work_item_count: 3,
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
                    }],
                    environments: vec![],
                }),
                visible_environments: vec![],
                repo_count: 1,
                work_item_count: 3,
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
            CommandValue::ResourceList(Box::new(ResourceJsonResponse {
                kind: "Convoy".into(),
                plural: "convoys".into(),
                namespace: "flotilla".into(),
                value: serde_json::json!({
                    "metadata": { "resourceVersion": "1" },
                    "items": [{ "apiVersion": "flotilla.work/v1", "kind": "Convoy", "metadata": { "name": "demo" }, "spec": {} }]
                }),
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
            })),
            CommandValue::ResourceWatchEvent(Box::new(ResourceWatchResponse {
                kind: "Convoy".into(),
                plural: "convoys".into(),
                namespace: "flotilla".into(),
                event: serde_json::json!({
                    "type": "ADDED",
                    "object": { "apiVersion": "flotilla.work/v1", "kind": "Convoy", "metadata": { "name": "demo" }, "spec": {} }
                }),
            })),
            CommandValue::ImageEnsured { image: crate::ImageId::new("sha256:abc123") },
            CommandValue::EnvironmentCreated { env_id: crate::EnvironmentId::new("env-1") },
            CommandValue::EnvironmentSpecRead {
                spec: crate::EnvironmentSpec {
                    image: crate::ImageSource::Registry("ubuntu:24.04".into()),
                    token_env_vars: vec!["GITHUB_TOKEN".into()],
                },
            },
            CommandValue::IssuePage(crate::issue_query::IssueResultPage { items: vec![], total: Some(10), has_more: true }),
            CommandValue::IssuesByIds { items: vec![] },
            CommandValue::ConvoyCreated { name: "my-convoy".into() },
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
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CreateWorkspaceForCheckout { checkout_path: PathBuf::from("/tmp"), label: "ws".into() },
            },
            Command {
                node_id: Some(NodeId::new("desktop")),
                provisioning_target: None,
                context_repo: Some(RepoSelector::Identity(repo_identity())),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from("/remote/repo/feat-x"), commands: vec![] },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Identity(repo_identity())),
                action: CommandAction::CreateWorkspaceFromPreparedTerminal {
                    target_node_id: NodeId::new("desktop"),
                    branch: "feat-x".into(),
                    checkout_path: PathBuf::from("/remote/repo/feat-x"),
                    attachable_set_id: None,
                    commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash".into())] }],
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::SelectWorkspace { ws_ref: "x".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("repo".into()),
                    target: CheckoutTarget::Branch("b".into()),
                    issue_ids: vec![],
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("b".into()) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus { branch: "b".into(), checkout_path: None, change_request_id: None },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::OpenChangeRequest { id: "1".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::CloseChangeRequest { id: "1".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::OpenIssue { id: "1".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::LinkIssuesToChangeRequest { change_request_id: "1".into(), issue_ids: vec![] },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::ArchiveSession { session_id: "s".into() },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::GenerateBranchName { issue_keys: vec![] },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyDelete { namespace: Some("flotilla".into()), name: "failed-convoy".into(), force: false },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))),
                action: CommandAction::TeleportSession { session_id: "s".into(), branch: None, checkout_key: None },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::TrackRepoPath { path: PathBuf::from("/tmp") },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::UntrackRepo { repo: RepoSelector::Path(PathBuf::from("/tmp")) },
            },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::Refresh { repo: None } },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoDetail { repo: RepoSelector::Path(PathBuf::from("/tmp")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoProviders { repo: RepoSelector::Path(PathBuf::from("/tmp")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryRepoWork { repo: RepoSelector::Path(PathBuf::from("/tmp")) },
            },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryHostList {} },
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::QueryProjectList {} },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostStatus { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryHostProviders { target_environment_id: EnvironmentId::host(HostId::new("desktop-host")) },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssues {
                    repo: RepoSelector::Query("test".into()),
                    params: crate::issue_query::IssueQuery::default(),
                    page: 1,
                    count: 50,
                },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueFetchByIds { repo: RepoSelector::Path(PathBuf::from("/repo")), ids: vec!["1".into()] },
            },
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueOpenInBrowser { repo: RepoSelector::Path(PathBuf::from("/repo")), id: "42".into() },
            },
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
}
