//! Domain decision for the host on which a command executes.

use flotilla_protocol::{qualified_path::HostId, CommandAction, NodeId};

use crate::in_process::{ExistingConvoyTarget, InProcessDaemon};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetReason {
    /// A new Convoy is admitted by the selected primary placement host.
    Admission,
    /// An existing Convoy or resource is changed at its authoritative home.
    RecordHome,
    /// A session operation is executed where that session is stored.
    CrewSessionHome,
    /// The caller selected a node for an action without an inferred home.
    Explicit,
    /// A read uses the dispatching daemon's query view.
    LocalRead,
    /// An action without a known remote home executes on the dispatching daemon.
    LocalMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetHost {
    /// Execute on the dispatching daemon.
    Local,
    /// Execute on an addressed node or known resource origin.
    Node(NodeId),
    /// Resolve a placement host identity to its connected node for delivery.
    Placement(HostId),
    /// Execute at a Convoy's home, retaining its last-seen context for errors.
    ConvoyHome(ExistingConvoyTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDelivery {
    Command,
    Steps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTarget {
    pub host: TargetHost,
    pub reason: TargetReason,
    pub delivery: RemoteDelivery,
}

#[derive(Debug)]
pub enum TargetError {
    Admission(String),
    RecordHome(String),
    CrewSessionHome(String),
    /// Produced by delivery when the resolved host has no usable route.
    Unreachable(String),
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(message) | Self::RecordHome(message) | Self::CrewSessionHome(message) | Self::Unreachable(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for TargetError {}

impl InProcessDaemon {
    pub async fn resolve_command_target(
        &self,
        action: &CommandAction,
        requested_node: Option<&NodeId>,
    ) -> Result<CommandTarget, TargetError> {
        use CommandAction as A;

        // Keep this exhaustive: adding a command requires choosing its execution
        // semantics here, before the transport sees it.
        let (mut reason, mut delivery) = match action {
            A::ConvoyStart { .. } | A::ConvoyCreate { .. } => (TargetReason::Admission, RemoteDelivery::Command),
            A::ConvoyDelete { .. }
            | A::ConvoyLink { .. }
            | A::ConvoyUnlink { .. }
            | A::ConvoyAbandon { .. }
            | A::ConvoyResume { .. }
            | A::ConvoyWithdrawPendingBrief { .. }
            | A::ConvoyWorkForceComplete { .. }
            | A::CrewComplete { .. }
            | A::CrewFail { .. }
            | A::CrewStall { .. }
            | A::CrewHandoff { .. }
            | A::CrewSupervise { .. }
            | A::DeliverCrewTurn { .. }
            | A::ResourceApply { .. }
            | A::ResourceManifestResolve { .. }
            | A::ConvoyEnsureRoll { .. }
            | A::ResourceReconcileNow { .. }
            | A::ResourceStatusPatch { .. }
            | A::ResourceDelete { .. }
            | A::RepositoryRemoteRemove { .. } => (TargetReason::RecordHome, RemoteDelivery::Command),
            A::ArchiveSession { .. } | A::TeleportSession { .. } => (TargetReason::CrewSessionHome, RemoteDelivery::Steps),
            A::QueryHostList { .. }
            | A::QueryProjectList { .. }
            | A::QueryCliList { .. }
            | A::QueryDispatchQueue { .. }
            | A::QueryHostStatus { .. }
            | A::QueryHostProviders { .. }
            | A::QueryFleetHealth { .. }
            | A::QueryFulfilmentList { .. }
            | A::QueryFleetList { .. }
            | A::QueryCrewList { .. }
            | A::QueryFleetReplicaSnapshot { .. }
            | A::QueryDaemonLogs { .. }
            | A::QueryExplainConvoy { .. }
            | A::QueryResourceList { .. }
            | A::QueryResourceGet { .. }
            | A::QueryResolveRepository { .. }
            | A::QueryRepoProviders { .. }
            | A::QueryIssues { .. }
            | A::QueryIssueFetchByIds { .. }
            | A::QueryIssueOpenInBrowser { .. }
            | A::Attach { .. }
            | A::AttachTransient { .. }
            | A::ResourceWatch { .. } => (TargetReason::LocalRead, RemoteDelivery::Command),
            A::CreateWorkspaceForCheckout { .. }
            | A::CreateWorkspaceFromPreparedTerminal { .. }
            | A::SelectWorkspace { .. }
            | A::PrepareTerminalForCheckout { .. }
            | A::Checkout { .. }
            | A::RemoveCheckout { .. }
            | A::FetchCheckoutStatus { .. }
            | A::OpenChangeRequest { .. }
            | A::CloseChangeRequest { .. }
            | A::MergeChangeRequest { .. }
            | A::OpenIssue { .. }
            | A::LinkIssuesToChangeRequest { .. }
            | A::GenerateBranchName { .. }
            | A::WorkflowTemplateApply { .. }
            | A::ProjectAdd { .. }
            | A::ProjectApply { .. }
            | A::ProjectRegister { .. }
            | A::ProjectRefresh { .. }
            | A::TrackRepoPath { .. }
            | A::UntrackRepo { .. }
            | A::Refresh { .. } => (TargetReason::LocalMutation, RemoteDelivery::Steps),
        };

        let host = match action {
            A::ConvoyStart { intent } => {
                let namespace = intent.namespace.clone().unwrap_or(self.provisioning_namespace().await);
                self.convoy_start_placement_host(&namespace, intent)
                    .await
                    .map_err(TargetError::Admission)?
                    .map_or(TargetHost::Local, TargetHost::Placement)
            }
            A::ConvoyCreate { placement_policy, .. } => {
                let namespace = self.provisioning_namespace().await;
                self.remote_placement_host(&namespace, placement_policy.as_deref())
                    .await
                    .map_err(TargetError::Admission)?
                    .map_or(TargetHost::Local, TargetHost::Placement)
            }
            A::ArchiveSession { session_id } | A::TeleportSession { session_id, .. } => {
                let session_action = A::ResourceReconcileNow {
                    namespace: self.provisioning_namespace().await,
                    kind: "TerminalSession".into(),
                    name: session_id.clone(),
                };
                if let Some(origin) = self.resource_mutation_origin(&session_action).await.map_err(TargetError::CrewSessionHome)? {
                    delivery = RemoteDelivery::Command;
                    TargetHost::Node(origin)
                } else if let Some(node) = requested_node {
                    reason = TargetReason::Explicit;
                    TargetHost::Node(node.clone())
                } else {
                    reason = TargetReason::LocalMutation;
                    TargetHost::Local
                }
            }
            _ => {
                if let Some(convoy) = self.resolve_existing_convoy_target(action).await.map_err(TargetError::RecordHome)? {
                    reason = TargetReason::RecordHome;
                    TargetHost::ConvoyHome(convoy)
                } else if let Some(origin) = self.resource_mutation_origin(action).await.map_err(TargetError::RecordHome)? {
                    reason = TargetReason::RecordHome;
                    TargetHost::Node(origin)
                } else if let Some(node) = requested_node {
                    reason = TargetReason::Explicit;
                    TargetHost::Node(node.clone())
                } else {
                    if reason == TargetReason::RecordHome {
                        reason = TargetReason::LocalMutation;
                    }
                    TargetHost::Local
                }
            }
        };
        Ok(CommandTarget { host, reason, delivery })
    }
}
