use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use flotilla_protocol::{CommandValue, DaemonEvent, HostName, RepoIdentity, StepStatus};
pub use flotilla_protocol::{Step, StepAction, StepExecutionContext, StepOutcome};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::path_context::ExecutionEnvironmentPath;

/// Resolves symbolic step actions into outcomes.
#[async_trait::async_trait]
pub trait StepResolver: Send + Sync {
    async fn resolve(
        &self,
        description: &str,
        context: &StepExecutionContext,
        action: StepAction,
        prior: &[StepOutcome],
    ) -> Result<StepOutcome, String>;
}

pub struct RemoteStepBatchRequest {
    pub command_id: u64,
    pub target_host: HostName,
    pub repo_identity: RepoIdentity,
    /// Requester-local repo path used when remapping remote progress into
    /// `DaemonEvent::CommandStepUpdate` for the UI. Remote execution itself
    /// resolves the actual repo root from `repo_identity`. This may be absent
    /// when the requester only has identity metadata for the repo.
    pub repo: Option<ExecutionEnvironmentPath>,
    /// Global step index of the first step in this batch on the requester.
    ///
    /// The executing host emits batch-relative progress indices. The requester
    /// uses this offset when remapping those updates into the global command
    /// timeline that the UI already understands.
    pub step_offset: usize,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteStepProgressUpdate {
    pub batch_step_index: usize,
    pub batch_step_count: usize,
    pub description: String,
    pub status: StepStatus,
}

#[async_trait::async_trait]
pub trait RemoteStepProgressSink: Send + Sync {
    async fn emit(&self, update: RemoteStepProgressUpdate);
}

#[async_trait::async_trait]
pub trait RemoteStepExecutor: Send + Sync {
    async fn execute_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
    ) -> Result<Vec<StepOutcome>, String>;

    async fn cancel_active_batch(&self, command_id: u64) -> Result<(), String>;
}

pub struct UnsupportedRemoteStepExecutor;

#[async_trait::async_trait]
impl RemoteStepExecutor for UnsupportedRemoteStepExecutor {
    async fn execute_batch(
        &self,
        request: RemoteStepBatchRequest,
        _progress_sink: Arc<dyn RemoteStepProgressSink>,
    ) -> Result<Vec<StepOutcome>, String> {
        Err(format!("remote step execution is not wired for host {}", request.target_host))
    }

    async fn cancel_active_batch(&self, _command_id: u64) -> Result<(), String> {
        Ok(())
    }
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
    local_host: HostName,
    repo_identity: RepoIdentity,
    repo: ExecutionEnvironmentPath,
    cancel: CancellationToken,
    event_tx: broadcast::Sender<DaemonEvent>,
    resolver: &dyn StepResolver,
) -> CommandValue {
    let remote_executor = UnsupportedRemoteStepExecutor;
    run_step_plan_with_remote_executor(plan, command_id, local_host, repo_identity, repo, cancel, event_tx, resolver, &remote_executor)
        .await
}

/// Execute a step plan with explicit remote-step handling.
#[allow(clippy::too_many_arguments)]
pub async fn run_step_plan_with_remote_executor(
    plan: StepPlan,
    command_id: u64,
    local_host: HostName,
    repo_identity: RepoIdentity,
    repo: ExecutionEnvironmentPath,
    cancel: CancellationToken,
    event_tx: broadcast::Sender<DaemonEvent>,
    resolver: &dyn StepResolver,
    remote_executor: &dyn RemoteStepExecutor,
) -> CommandValue {
    let step_count = plan.steps.len();
    info!(%command_id, %step_count, %local_host, "running step plan");
    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let steps = plan.steps;
    let mut i = 0usize;

    while i < step_count {
        if cancel.is_cancelled() {
            return CommandValue::Cancelled;
        }

        let step = steps[i].clone();
        let step_target = step.host.host_name().clone();
        debug!(%command_id, %step_target, %local_host, step_index = i, desc = %step.description, "step dispatch");

        if step_target == local_host {
            emit_step_update(
                &event_tx,
                command_id,
                local_host.clone(),
                repo_identity.clone(),
                Some(repo.as_path().to_path_buf()),
                i,
                step_count,
                step.description.clone(),
                StepStatus::Started,
            );

            let outcome = resolver.resolve(&step.description, &step.host, step.action, &outcomes).await;

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
                    emit_step_update(
                        &event_tx,
                        command_id,
                        local_host.clone(),
                        repo_identity.clone(),
                        Some(repo.as_path().to_path_buf()),
                        i,
                        step_count,
                        step.description.clone(),
                        status,
                    );
                    outcomes.push(step_outcome);
                }
                Err(e) => {
                    emit_step_update(
                        &event_tx,
                        command_id,
                        local_host.clone(),
                        repo_identity.clone(),
                        Some(repo.as_path().to_path_buf()),
                        i,
                        step_count,
                        step.description.clone(),
                        StepStatus::Failed { message: e.clone() },
                    );
                    return prior_result_or_error(&outcomes, e);
                }
            }
            i += 1;
        } else {
            let target_host = step_target;
            {
                let segment_start = i;
                let mut segment_steps = vec![step];
                i += 1;
                while i < step_count {
                    if *steps[i].host.host_name() == target_host {
                        segment_steps.push(steps[i].clone());
                        i += 1;
                    } else {
                        break;
                    }
                }

                let progress_sink = Arc::new(EventForwardingProgressSink {
                    command_id,
                    host: target_host.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: Some(repo.clone()),
                    step_offset: segment_start,
                    step_count,
                    event_tx: event_tx.clone(),
                    state: Mutex::new(RemoteProgressState::default()),
                });
                let request = RemoteStepBatchRequest {
                    command_id,
                    target_host: target_host.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: Some(repo.clone()),
                    step_offset: segment_start,
                    steps: segment_steps,
                };

                let batch = remote_executor.execute_batch(request, progress_sink.clone());
                tokio::pin!(batch);

                let (cancelled_during_batch, outcome) = tokio::select! {
                    outcome = &mut batch => (false, outcome),
                    _ = cancel.cancelled() => {
                        let outcome = match remote_executor.cancel_active_batch(command_id).await {
                            Ok(()) => tokio::time::timeout(Duration::from_secs(5), &mut batch)
                                .await
                                .unwrap_or_else(|_| Err("timed out waiting for remote batch cancellation".into())),
                            Err(message) => Err(message),
                        };
                        (true, outcome)
                    }
                };

                if cancelled_during_batch {
                    return CommandValue::Cancelled;
                }

                match outcome {
                    Ok(step_outcomes) => outcomes.extend(step_outcomes),
                    Err(e) => {
                        if let Some(failure) = progress_sink.synthesized_failure(e.clone()) {
                            emit_step_update(
                                &event_tx,
                                command_id,
                                target_host.clone(),
                                repo_identity.clone(),
                                Some(repo.as_path().to_path_buf()),
                                segment_start + failure.batch_step_index,
                                step_count,
                                failure.description,
                                StepStatus::Failed { message: e.clone() },
                            );
                        }
                        return prior_result_or_error(&outcomes, e);
                    }
                }
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

#[allow(clippy::too_many_arguments)]
fn emit_step_update(
    event_tx: &broadcast::Sender<DaemonEvent>,
    command_id: u64,
    host: HostName,
    repo_identity: RepoIdentity,
    repo: Option<std::path::PathBuf>,
    step_index: usize,
    step_count: usize,
    description: String,
    status: StepStatus,
) {
    debug!(%command_id, %host, step_index, step_count, %description, ?status, "emit_step_update");
    let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
        command_id,
        host,
        repo_identity,
        repo,
        step_index,
        step_count,
        description,
        status,
    });
}

fn prior_result_or_error(outcomes: &[StepOutcome], error: String) -> CommandValue {
    let prior_result = outcomes.iter().rev().find_map(|o| match o {
        StepOutcome::CompletedWith(r) => Some(r.clone()),
        _ => None,
    });
    prior_result.unwrap_or(CommandValue::Error { message: error })
}

struct EventForwardingProgressSink {
    command_id: u64,
    host: HostName,
    repo_identity: RepoIdentity,
    repo: Option<ExecutionEnvironmentPath>,
    step_offset: usize,
    step_count: usize,
    event_tx: broadcast::Sender<DaemonEvent>,
    state: Mutex<RemoteProgressState>,
}

#[derive(Default)]
struct RemoteProgressState {
    latest_batch_step_index: usize,
    latest_description: Option<String>,
    failed_emitted: bool,
}

struct SynthesizedRemoteFailure {
    batch_step_index: usize,
    description: String,
}

impl EventForwardingProgressSink {
    fn synthesized_failure(&self, message: String) -> Option<SynthesizedRemoteFailure> {
        let state = self.state.lock().expect("progress state mutex poisoned");
        if state.failed_emitted {
            return None;
        }

        Some(SynthesizedRemoteFailure {
            batch_step_index: state.latest_batch_step_index,
            description: state.latest_description.clone().unwrap_or(message),
        })
    }
}

#[async_trait::async_trait]
impl RemoteStepProgressSink for EventForwardingProgressSink {
    async fn emit(&self, update: RemoteStepProgressUpdate) {
        debug!(command_id = self.command_id, %self.host, batch_step_index = update.batch_step_index, ?update.status, %update.description, "remote progress received");
        {
            let mut state = self.state.lock().expect("progress state mutex poisoned");
            state.latest_batch_step_index = update.batch_step_index;
            state.latest_description = Some(update.description.clone());
            if matches!(update.status, StepStatus::Failed { .. }) {
                state.failed_emitted = true;
            }
        }
        emit_step_update(
            &self.event_tx,
            self.command_id,
            self.host.clone(),
            self.repo_identity.clone(),
            self.repo.as_ref().map(|repo| repo.as_path().to_path_buf()),
            self.step_offset + update.batch_step_index,
            self.step_count,
            update.description,
            update.status,
        );
    }
}

#[cfg(test)]
mod tests;
