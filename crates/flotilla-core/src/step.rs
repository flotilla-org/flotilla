use flotilla_protocol::{
    AttachableSetId, CommandValue, DaemonEvent, HostName, HostPath, PreparedTerminalCommand, RepoIdentity, ResolvedPaneCommand, StepStatus,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{executor::checkout::CheckoutIntent, path_context::ExecutionEnvironmentPath};

/// Outcome of a single step execution.
#[derive(Debug, Clone)]
pub enum StepOutcome {
    /// Step completed successfully, no specific result to report.
    Completed,
    /// Step completed and wants to override the final CommandValue.
    CompletedWith(CommandValue),
    /// Inter-step data visible to later steps but excluded from the final result.
    Produced(CommandValue),
    /// Step determined its work was already done and skipped.
    Skipped,
}

/// Which host a step should execute on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepHost {
    /// Run on the same host as the stepper (the daemon executing the plan).
    Local,
    /// Run on a specific named remote host.
    Remote(HostName),
}

/// A symbolic action that the step runner resolves at execution time.
pub enum StepAction {
    // Checkout lifecycle
    CreateCheckout {
        branch: String,
        create_branch: bool,
        intent: CheckoutIntent,
        issue_ids: Vec<(String, String)>,
    },
    LinkIssuesToBranch {
        branch: String,
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        branch: String,
        deleted_checkout_paths: Vec<HostPath>,
    },

    // Workspace (existing)
    /// Create a workspace for a checkout path produced by a prior step.
    CreateWorkspaceForCheckout {
        label: String,
        checkout_path: Option<ExecutionEnvironmentPath>,
    },

    // Teleport
    ResolveAttachCommand {
        session_id: String,
    },
    EnsureCheckoutForTeleport {
        branch: Option<String>,
        checkout_key: Option<ExecutionEnvironmentPath>,
        initial_path: Option<ExecutionEnvironmentPath>,
    },
    CreateTeleportWorkspace {
        /// Unused by the current resolver, but kept for batch 2: remote step
        /// routing may need it to re-resolve the attach command on the target host.
        session_id: String,
        branch: Option<String>,
    },

    // Session
    ArchiveSession {
        session_id: String,
    },
    GenerateBranchName {
        issue_keys: Vec<String>,
    },

    // Workspace lifecycle (new)
    CreateWorkspaceFromPreparedTerminal {
        target_host: HostName,
        branch: String,
        checkout_path: ExecutionEnvironmentPath,
        attachable_set_id: Option<AttachableSetId>,
        commands: Vec<ResolvedPaneCommand>,
    },
    SelectWorkspace {
        ws_ref: String,
    },
    PrepareTerminalForCheckout {
        checkout_path: ExecutionEnvironmentPath,
        commands: Vec<PreparedTerminalCommand>,
    },

    // Query
    FetchCheckoutStatus {
        branch: String,
        checkout_path: Option<ExecutionEnvironmentPath>,
        change_request_id: Option<String>,
    },

    // External interactions
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

    /// Test-only no-op action resolved by test harness resolvers.
    #[cfg(test)]
    Noop,
}

/// Resolves symbolic step actions into outcomes.
#[async_trait::async_trait]
pub trait StepResolver: Send + Sync {
    async fn resolve(&self, description: &str, action: StepAction, prior: &[StepOutcome]) -> Result<StepOutcome, String>;
}

/// A single step in a multi-step command.
pub struct Step {
    pub description: String,
    pub host: StepHost,
    pub action: StepAction,
}

/// A plan of steps to execute for a command.
pub struct StepPlan {
    pub steps: Vec<Step>,
}

impl StepPlan {
    pub fn new(steps: Vec<Step>) -> Self {
        Self { steps }
    }
}

/// Execute a step plan, emitting progress events and checking cancellation between steps.
#[allow(clippy::too_many_arguments)]
pub async fn run_step_plan(
    plan: StepPlan,
    command_id: u64,
    host: HostName,
    repo_identity: RepoIdentity,
    repo: ExecutionEnvironmentPath,
    cancel: CancellationToken,
    event_tx: broadcast::Sender<DaemonEvent>,
    resolver: &dyn StepResolver,
) -> CommandValue {
    let step_count = plan.steps.len();
    let mut outcomes: Vec<StepOutcome> = Vec::new();

    for (i, step) in plan.steps.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return CommandValue::Cancelled;
        }

        let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
            command_id,
            host: host.clone(),
            repo_identity: repo_identity.clone(),
            repo: repo.as_path().to_path_buf(),
            step_index: i,
            step_count,
            description: step.description.clone(),
            status: StepStatus::Started,
        });

        let outcome = resolver.resolve(&step.description, step.action, &outcomes).await;

        // Cancellation wins over a successful in-flight step, but provider
        // errors still surface so we don't hide the underlying failure.
        if cancel.is_cancelled() && outcome.is_ok() {
            return CommandValue::Cancelled;
        }

        match outcome {
            Ok(step_outcome) => {
                let status = match &step_outcome {
                    StepOutcome::Skipped => StepStatus::Skipped,
                    _ => StepStatus::Succeeded,
                };
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    host: host.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: repo.as_path().to_path_buf(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status,
                });
                outcomes.push(step_outcome);
            }
            Err(e) => {
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    host: host.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: repo.as_path().to_path_buf(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Failed { message: e.clone() },
                });
                // If a prior step produced a meaningful result, preserve it.
                // The failure is already reported via the StepFailed event.
                let prior_result = outcomes.iter().rev().find_map(|o| match o {
                    StepOutcome::CompletedWith(r) => Some(r.clone()),
                    _ => None,
                });
                return prior_result.unwrap_or(CommandValue::Error { message: e });
            }
        }
    }

    // Return the last meaningful result, or Ok if no step produced one
    outcomes
        .into_iter()
        .rev()
        .find_map(|o| match o {
            StepOutcome::CompletedWith(r) => Some(r),
            _ => None,
        })
        .unwrap_or(CommandValue::Ok)
}

#[cfg(test)]
mod tests;
