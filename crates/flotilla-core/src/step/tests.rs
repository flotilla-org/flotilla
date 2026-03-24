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
        Ok(StepOutcome::CompletedWith(CommandValue::CheckoutCreated { branch: "feat/x".into(), path: PathBuf::from("/repo/wt-feat-x") })),
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
        Ok(StepOutcome::CompletedWith(CommandValue::CheckoutCreated { branch: "feat/x".into(), path: PathBuf::from("/repo/wt-feat-x") })),
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
