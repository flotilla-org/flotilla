use std::path::PathBuf;

use flotilla_protocol::{CommandValue, DaemonEvent, HostName, HostPath, ManagedTerminalId, RepoIdentity, StepStatus};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::executor::checkout::CheckoutIntent;

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
        terminal_keys: Vec<ManagedTerminalId>,
        deleted_checkout_paths: Vec<HostPath>,
    },

    // Workspace (existing)
    /// Create a workspace for a checkout path produced by a prior step.
    CreateWorkspaceForCheckout {
        label: String,
    },

    // Teleport
    ResolveAttachCommand {
        session_id: String,
    },
    EnsureCheckoutForTeleport {
        branch: Option<String>,
        checkout_key: Option<PathBuf>,
        initial_path: Option<PathBuf>,
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
    repo: PathBuf,
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
            repo: repo.clone(),
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
                    repo: repo.clone(),
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
                    repo: repo.clone(),
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
mod tests {
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::*;

    struct TestResolver {
        outcomes: std::sync::Mutex<Vec<Result<StepOutcome, String>>>,
    }

    impl TestResolver {
        fn new(outcomes: Vec<Result<StepOutcome, String>>) -> Self {
            Self { outcomes: std::sync::Mutex::new(outcomes) }
        }
    }

    #[async_trait::async_trait]
    impl StepResolver for TestResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            self.outcomes.lock().unwrap().remove(0)
        }
    }

    fn make_step(desc: &str) -> Step {
        Step { description: desc.to_string(), host: StepHost::Local, action: StepAction::Noop }
    }

    fn setup() -> (CancellationToken, broadcast::Sender<DaemonEvent>) {
        let (tx, _rx) = broadcast::channel(64);
        (CancellationToken::new(), tx)
    }

    #[tokio::test]
    async fn all_steps_succeed() {
        let (cancel, tx) = setup();
        let mut rx = tx.subscribe();
        let resolver = TestResolver::new(vec![Ok(StepOutcome::Completed), Ok(StepOutcome::Completed)]);
        let plan = StepPlan::new(vec![make_step("step-a"), make_step("step-b")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Ok);

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
        let resolver = TestResolver::new(vec![Ok(StepOutcome::Completed), Err("boom".into()), Ok(StepOutcome::Completed)]);
        let plan = StepPlan::new(vec![make_step("step-a"), make_step("step-b"), make_step("step-c")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Error { message: "boom".into() });
    }

    #[tokio::test]
    async fn cancellation_before_step() {
        let (cancel, tx) = setup();
        cancel.cancel();
        let resolver = TestResolver::new(vec![Ok(StepOutcome::Completed)]);
        let plan = StepPlan::new(vec![make_step("step-a")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_during_running_step_returns_cancelled() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        struct BlockingResolver {
            started: Arc<Notify>,
            release: Arc<Notify>,
        }

        #[async_trait::async_trait]
        impl StepResolver for BlockingResolver {
            async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
                self.started.notify_waiters();
                self.release.notified().await;
                Ok(StepOutcome::Completed)
            }
        }

        let (cancel, tx) = setup();
        let resolver = BlockingResolver { started: Arc::clone(&started), release: Arc::clone(&release) };
        let plan = StepPlan::new(vec![make_step("step-a")]);

        let cancel2 = cancel.clone();
        let task = tokio::spawn(async move {
            run_step_plan(
                plan,
                1,
                HostName::local(),
                RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
                PathBuf::from("/repo"),
                cancel2,
                tx,
                &resolver,
            )
            .await
        });
        started.notified().await;
        cancel.cancel();
        release.notify_waiters();

        let result = task.await.expect("task should join");
        assert_eq!(result, CommandValue::Cancelled);
    }

    #[tokio::test]
    async fn skipped_step_continues() {
        let (cancel, tx) = setup();
        let resolver = TestResolver::new(vec![Ok(StepOutcome::Skipped), Ok(StepOutcome::Completed)]);
        let plan = StepPlan::new(vec![make_step("step-a"), make_step("step-b")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Ok);
    }

    #[tokio::test]
    async fn completed_with_overrides_result() {
        let (cancel, tx) = setup();
        let resolver = TestResolver::new(vec![
            Ok(StepOutcome::CompletedWith(CommandValue::CheckoutCreated {
                branch: "feat/x".into(),
                path: PathBuf::from("/repo/wt-feat-x"),
            })),
            Ok(StepOutcome::Completed),
        ]);
        let plan = StepPlan::new(vec![make_step("step-a"), make_step("step-b")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::CheckoutCreated { branch: "feat/x".into(), path: PathBuf::from("/repo/wt-feat-x") });
    }

    #[tokio::test]
    async fn empty_plan_returns_ok() {
        let (cancel, tx) = setup();
        let resolver = TestResolver::new(vec![]);
        let plan = StepPlan::new(vec![]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Ok);
    }

    #[tokio::test]
    async fn symbolic_step_action_succeeds() {
        let (cancel, tx) = setup();
        let resolver = TestResolver::new(vec![Ok(StepOutcome::Completed)]);
        let plan = StepPlan::new(vec![make_step("symbolic step")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Ok);
    }

    #[tokio::test]
    async fn produced_does_not_override_final_result() {
        let (cancel, tx) = setup();
        let resolver = TestResolver::new(vec![
            Ok(StepOutcome::Produced(CommandValue::AttachCommandResolved { command: "attach cmd".into() })),
            Ok(StepOutcome::Completed),
        ]);
        let plan = StepPlan::new(vec![make_step("step-a"), make_step("step-b")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::Ok);
    }

    #[tokio::test]
    async fn later_failure_preserves_earlier_completed_with() {
        let (cancel, tx) = setup();
        let resolver = TestResolver::new(vec![
            Ok(StepOutcome::CompletedWith(CommandValue::CheckoutCreated {
                branch: "feat/x".into(),
                path: PathBuf::from("/repo/wt-feat-x"),
            })),
            Err("workspace failed".into()),
        ]);
        let plan = StepPlan::new(vec![make_step("step-a"), make_step("step-b")]);

        let result = run_step_plan(
            plan,
            1,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            PathBuf::from("/repo"),
            cancel,
            tx,
            &resolver,
        )
        .await;
        assert_eq!(result, CommandValue::CheckoutCreated { branch: "feat/x".into(), path: PathBuf::from("/repo/wt-feat-x") });
    }
}
