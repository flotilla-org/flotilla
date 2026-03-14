use std::{future::Future, path::PathBuf, pin::Pin};

use flotilla_protocol::{CommandResult, DaemonEvent, HostName, StepStatus};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Outcome of a single step execution.
pub enum StepOutcome {
    /// Step completed successfully, no specific result to report.
    Completed,
    /// Step completed and wants to override the final CommandResult.
    CompletedWith(CommandResult),
    /// Step determined its work was already done and skipped.
    Skipped,
}

/// The future returned by a step's action closure.
pub type StepFuture = Pin<Box<dyn Future<Output = Result<StepOutcome, String>> + Send>>;

/// A single step in a multi-step command.
pub struct Step {
    pub description: String,
    pub action: Box<dyn FnOnce() -> StepFuture + Send>,
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
pub async fn run_step_plan(
    plan: StepPlan,
    command_id: u64,
    host: HostName,
    repo: PathBuf,
    cancel: CancellationToken,
    event_tx: broadcast::Sender<DaemonEvent>,
) -> CommandResult {
    let step_count = plan.steps.len();
    let mut final_result = CommandResult::Ok;

    for (i, step) in plan.steps.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return CommandResult::Cancelled;
        }

        let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
            command_id,
            host: host.clone(),
            repo: repo.clone(),
            step_index: i,
            step_count,
            description: step.description.clone(),
            status: StepStatus::Started,
        });

        let outcome = (step.action)().await;
        if cancel.is_cancelled() {
            if outcome.is_ok() {
                return CommandResult::Cancelled;
            }
        }

        match outcome {
            Ok(StepOutcome::Completed) => {
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    host: host.clone(),
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Succeeded,
                });
            }
            Ok(StepOutcome::CompletedWith(result)) => {
                final_result = result;
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    host: host.clone(),
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Succeeded,
                });
            }
            Ok(StepOutcome::Skipped) => {
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    host: host.clone(),
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Skipped,
                });
            }
            Err(e) => {
                let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
                    command_id,
                    host: host.clone(),
                    repo: repo.clone(),
                    step_index: i,
                    step_count,
                    description: step.description.clone(),
                    status: StepStatus::Failed { message: e.clone() },
                });
                return CommandResult::Error { message: e };
            }
        }
    }

    final_result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use tokio::sync::Notify;

    fn make_step(desc: &str, outcome: Result<StepOutcome, String>) -> Step {
        let outcome = Arc::new(tokio::sync::Mutex::new(Some(outcome)));
        Step {
            description: desc.to_string(),
            action: Box::new(move || {
                let outcome = Arc::clone(&outcome);
                Box::pin(async move { outcome.lock().await.take().expect("step called twice") })
            }),
        }
    }

    fn setup() -> (CancellationToken, broadcast::Sender<DaemonEvent>) {
        let (tx, _rx) = broadcast::channel(64);
        (CancellationToken::new(), tx)
    }

    #[tokio::test]
    async fn all_steps_succeed() {
        let (cancel, tx) = setup();
        let mut rx = tx.subscribe();
        let plan = StepPlan::new(vec![make_step("step-a", Ok(StepOutcome::Completed)), make_step("step-b", Ok(StepOutcome::Completed))]);

        let result = run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Ok);

        // Should have 4 events: Started+Succeeded for each step
        let mut events = vec![];
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }
        assert_eq!(events.len(), 4);
    }

    #[tokio::test]
    async fn step_failure_stops_execution() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![
            make_step("step-a", Ok(StepOutcome::Completed)),
            make_step("step-b", Err("boom".into())),
            make_step("step-c", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Error { message: "boom".into() });
    }

    #[tokio::test]
    async fn cancellation_before_step() {
        let (cancel, tx) = setup();
        cancel.cancel();
        let plan = StepPlan::new(vec![make_step("step-a", Ok(StepOutcome::Completed))]);

        let result = run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_during_running_step_returns_cancelled() {
        let (cancel, tx) = setup();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let plan = StepPlan::new(vec![Step {
            description: "step-a".to_string(),
            action: Box::new({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move || {
                    Box::pin(async move {
                        started.notify_waiters();
                        release.notified().await;
                        Ok(StepOutcome::Completed)
                    })
                }
            }),
        }]);

        let task = tokio::spawn(run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel.clone(), tx));
        started.notified().await;
        cancel.cancel();
        release.notify_waiters();

        let result = task.await.expect("task should join");
        assert_eq!(result, CommandResult::Cancelled);
    }

    #[tokio::test]
    async fn skipped_step_continues() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![make_step("step-a", Ok(StepOutcome::Skipped)), make_step("step-b", Ok(StepOutcome::Completed))]);

        let result = run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Ok);
    }

    #[tokio::test]
    async fn completed_with_overrides_result() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![
            make_step(
                "step-a",
                Ok(StepOutcome::CompletedWith(CommandResult::CheckoutCreated {
                    branch: "feat/x".into(),
                    path: PathBuf::from("/repo/wt-feat-x"),
                })),
            ),
            make_step("step-b", Ok(StepOutcome::Completed)),
        ]);

        let result = run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::CheckoutCreated { branch: "feat/x".into(), path: PathBuf::from("/repo/wt-feat-x") });
    }

    #[tokio::test]
    async fn empty_plan_returns_ok() {
        let (cancel, tx) = setup();
        let plan = StepPlan::new(vec![]);

        let result = run_step_plan(plan, 1, HostName::local(), PathBuf::from("/repo"), cancel, tx).await;
        assert_eq!(result, CommandResult::Ok);
    }
}
