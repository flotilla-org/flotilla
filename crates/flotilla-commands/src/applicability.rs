//! Command applicability for interactive surfaces. This belongs beside the
//! command registry so serialized protocol actions carry no UI policy.

use flotilla_protocol::CommandAction;

use crate::Resolved;

/// Whether a registry noun can produce a command with a TUI-visible effect.
/// Names are the canonical clap subcommand names from `NounCommand`.
pub fn tui_actionable_noun(name: &str) -> bool {
    !matches!(name, "dispatch" | "fulfilment")
}

/// Whether a resolved command can be dispatched from a TUI palette.
pub fn tui_actionable_resolved(resolved: &Resolved) -> bool {
    match resolved {
        Resolved::HostQuery { .. } => false,
        Resolved::Ready(command) | Resolved::NeedsContext { command, .. } => tui_actionable_action(&command.action),
    }
}

/// Whether the TUI can present the effect of this typed action. Keep this
/// match exhaustive: adding an action requires an explicit applicability
/// decision, including for newly introduced query actions.
pub fn tui_actionable_action(action: &CommandAction) -> bool {
    match action {
        CommandAction::FetchCheckoutStatus { .. }
        | CommandAction::GenerateBranchName { .. }
        | CommandAction::QueryIssues { .. }
        | CommandAction::QueryIssueFetchByIds { .. }
        | CommandAction::QueryResolveRepository { .. }
        | CommandAction::QueryRepoProviders { .. }
        | CommandAction::QueryHostList {}
        | CommandAction::QueryExplainProject { .. }
        | CommandAction::QueryProjectList {}
        | CommandAction::QueryCliList { .. }
        | CommandAction::QueryDispatchBoard { .. }
        | CommandAction::QueryDispatchQueue { .. }
        | CommandAction::QueryHostStatus { .. }
        | CommandAction::QueryHostProviders { .. }
        | CommandAction::QueryFleetHealth {}
        | CommandAction::FleetPostInstall { .. }
        | CommandAction::QueryFulfilmentList {}
        | CommandAction::QueryFleetList { .. }
        | CommandAction::QueryCrewStalls { .. }
        | CommandAction::QueryPromiseQueue { .. }
        | CommandAction::QueryCrewCapabilities { .. }
        | CommandAction::QueryMessageContacts { .. }
        | CommandAction::QueryCrewList { .. }
        | CommandAction::QueryDaemonLogs { .. }
        | CommandAction::QueryExplainConvoy { .. }
        | CommandAction::QueryResourceDigest { .. }
        | CommandAction::QueryResourceList { .. }
        | CommandAction::QueryResourceGet { .. }
        | CommandAction::ResourceWatch { .. } => false,

        // Controller transport commands are unavailable to interactive surfaces.
        CommandAction::Attach { .. }
        | CommandAction::AttachTransient { .. }
        | CommandAction::Checkout { .. }
        | CommandAction::RemoveCheckout { .. }
        | CommandAction::OpenChangeRequest { .. }
        | CommandAction::CloseChangeRequest { .. }
        | CommandAction::MergeChangeRequest { .. }
        | CommandAction::OpenIssue { .. }
        | CommandAction::LinkIssuesToChangeRequest { .. }
        | CommandAction::ArchiveSession { .. }
        | CommandAction::ConvoyWorkForceComplete { .. }
        | CommandAction::ConvoyDelete { .. }
        | CommandAction::ConvoyLink { .. }
        | CommandAction::ConvoyUnlink { .. }
        | CommandAction::ConvoyAbandon { .. }
        | CommandAction::ConvoyResume { .. }
        | CommandAction::ConvoyWithdrawPendingBrief { .. }
        | CommandAction::CrewHandoff { .. }
        | CommandAction::CrewPromise { .. }
        | CommandAction::PromiseVerdict { .. }
        | CommandAction::CrewComplete { .. }
        | CommandAction::CrewFail { .. }
        | CommandAction::CrewStall { .. }
        | CommandAction::CrewSupervise { .. }
        | CommandAction::ConvoyCreate { .. }
        | CommandAction::ConvoyStart { .. }
        | CommandAction::WorkflowTemplateApply { .. }
        | CommandAction::ProjectAdd { .. }
        | CommandAction::ProjectApply { .. }
        | CommandAction::ProjectRegister { .. }
        | CommandAction::ProjectRefresh { .. }
        | CommandAction::TrackRepoPath { .. }
        | CommandAction::UntrackRepo { .. }
        | CommandAction::RepositoryRemoteRemove { .. }
        | CommandAction::Refresh { .. }
        | CommandAction::QueryIssueOpenInBrowser { .. }
        | CommandAction::ArtifactReserveLedgerComment { .. }
        | CommandAction::ResourceApply { .. }
        | CommandAction::ResourceManifestResolve { .. }
        | CommandAction::ConvoyEnsureRoll { .. }
        | CommandAction::ResourceReconcileNow { .. }
        | CommandAction::MessageFailBatch { .. }
        | CommandAction::ResourceStatusPatch { .. }
        | CommandAction::ResourceDelete { .. } => true,
    }
}
