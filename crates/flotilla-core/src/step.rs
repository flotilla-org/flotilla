use flotilla_protocol::{CommandValue, DaemonEvent, HostName, RepoIdentity, StepStatus};
pub use flotilla_protocol::{Step, StepAction, StepHost, StepOutcome};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::path_context::ExecutionEnvironmentPath;

/// Resolves symbolic step actions into outcomes.
#[async_trait::async_trait]
pub trait StepResolver: Send + Sync {
    async fn resolve(&self, description: &str, action: StepAction, prior: &[StepOutcome]) -> Result<StepOutcome, String>;
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
