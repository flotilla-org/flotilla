use std::{path::PathBuf, sync::Arc};

use tokio::sync::{Mutex, Notify};

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

#[derive(Clone)]
struct TestRemoteExecutor {
    batches: Arc<Mutex<Vec<TestRemoteBatch>>>,
    cancelled: Arc<Mutex<Vec<u64>>>,
    active_wait_for_cancel: Arc<Mutex<Option<Arc<Notify>>>>,
}

struct TestRemoteBatch {
    assert_host: HostName,
    progress: Vec<RemoteStepProgressUpdate>,
    wait_for_cancel: Option<Arc<Notify>>,
    result: Result<Vec<StepOutcome>, String>,
}

impl TestRemoteExecutor {
    fn new(batches: Vec<TestRemoteBatch>) -> Self {
        Self {
            batches: Arc::new(Mutex::new(batches)),
            cancelled: Arc::new(Mutex::new(Vec::new())),
            active_wait_for_cancel: Arc::new(Mutex::new(None)),
        }
    }

    async fn cancelled_commands(&self) -> Vec<u64> {
        self.cancelled.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl RemoteStepExecutor for TestRemoteExecutor {
    async fn execute_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
    ) -> Result<Vec<StepOutcome>, String> {
        let batch = self.batches.lock().await.remove(0);
        assert_eq!(request.target_host, batch.assert_host);

        for update in batch.progress {
            progress_sink.emit(update).await;
        }

        if let Some(wait_for_cancel) = batch.wait_for_cancel {
            *self.active_wait_for_cancel.lock().await = Some(Arc::clone(&wait_for_cancel));
            wait_for_cancel.notified().await;
            *self.active_wait_for_cancel.lock().await = None;
        }

        batch.result
    }

    async fn cancel_active_batch(&self, command_id: u64) -> Result<(), String> {
        self.cancelled.lock().await.push(command_id);
        if let Some(wait_for_cancel) = self.active_wait_for_cancel.lock().await.clone() {
            wait_for_cancel.notify_waiters();
        }
        Ok(())
    }
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
            ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
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
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &resolver,
    )
    .await;
    assert_eq!(result, CommandValue::CheckoutCreated { branch: "feat/x".into(), path: PathBuf::from("/repo/wt-feat-x") });
}

#[tokio::test]
async fn local_step_consumes_produced_outcome_from_remote_step() {
    struct PriorAssertingResolver;

    #[async_trait::async_trait]
    impl StepResolver for PriorAssertingResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            assert_eq!(prior, &[StepOutcome::Produced(CommandValue::AttachCommandResolved { command: "attach remote".into() })]);
            Ok(StepOutcome::Completed)
        }
    }

    let (cancel, tx) = setup();
    let plan = StepPlan::new(vec![
        Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
        make_step("local"),
    ]);
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![],
        wait_for_cancel: None,
        result: Ok(vec![StepOutcome::Produced(CommandValue::AttachCommandResolved { command: "attach remote".into() })]),
    }]);

    let result = run_step_plan_with_remote_executor(
        plan,
        1,
        HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &PriorAssertingResolver,
        &remote,
    )
    .await;

    assert_eq!(result, CommandValue::Ok);
}

#[tokio::test]
async fn remote_failure_stops_execution() {
    struct PanicResolver;

    #[async_trait::async_trait]
    impl StepResolver for PanicResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            panic!("local resolver should not be called after remote failure");
        }
    }

    let (cancel, tx) = setup();
    let mut rx = tx.subscribe();
    let plan = StepPlan::new(vec![
        Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
        make_step("local"),
    ]);
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 1,
                description: "remote".into(),
                status: StepStatus::Started,
            },
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 1,
                description: "remote".into(),
                status: StepStatus::Failed { message: "boom".into() },
            },
        ],
        wait_for_cancel: None,
        result: Err("boom".into()),
    }]);

    let result = run_step_plan_with_remote_executor(
        plan,
        1,
        HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &PanicResolver,
        &remote,
    )
    .await;

    assert_eq!(result, CommandValue::Error { message: "boom".into() });

    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(events.iter().all(|event| match event {
        DaemonEvent::CommandStepUpdate { step_index, .. } => *step_index == 0,
        _ => true,
    }));
}

#[tokio::test]
async fn remote_error_emits_failed_step_update_without_progress_failure() {
    struct PanicResolver;

    #[async_trait::async_trait]
    impl StepResolver for PanicResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            panic!("local resolver should not be called after remote failure");
        }
    }

    let (cancel, tx) = setup();
    let mut rx = tx.subscribe();
    let plan = StepPlan::new(vec![
        Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
        make_step("local"),
    ]);
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![RemoteStepProgressUpdate {
            batch_step_index: 0,
            batch_step_count: 1,
            description: "remote".into(),
            status: StepStatus::Started,
        }],
        wait_for_cancel: None,
        result: Err("boom".into()),
    }]);

    let result = run_step_plan_with_remote_executor(
        plan,
        1,
        HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &PanicResolver,
        &remote,
    )
    .await;

    assert_eq!(result, CommandValue::Error { message: "boom".into() });

    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(events.iter().any(|event| {
        matches!(
            event,
            DaemonEvent::CommandStepUpdate {
                host,
                step_index: 0,
                description,
                status: StepStatus::Failed { message },
                ..
            } if host == &HostName::new("feta") && description == "remote" && message == "boom"
        )
    }));
}

#[tokio::test]
async fn remote_error_uses_latest_started_step_for_multi_step_batch() {
    let (cancel, tx) = setup();
    let mut rx = tx.subscribe();
    let resolver = TestResolver::new(vec![Ok(StepOutcome::Completed)]);
    let plan = StepPlan::new(vec![
        make_step("local"),
        Step { description: "remote-a".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
        Step { description: "remote-b".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
    ]);
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 2,
                description: "remote-a".into(),
                status: StepStatus::Started,
            },
            RemoteStepProgressUpdate {
                batch_step_index: 1,
                batch_step_count: 2,
                description: "remote-b".into(),
                status: StepStatus::Started,
            },
        ],
        wait_for_cancel: None,
        result: Err("boom".into()),
    }]);

    let result = run_step_plan_with_remote_executor(
        plan,
        1,
        HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &resolver,
        &remote,
    )
    .await;

    assert_eq!(result, CommandValue::Error { message: "boom".into() });

    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let failure_events: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event {
            DaemonEvent::CommandStepUpdate { host, step_index, description, status, .. } => Some((host, step_index, description, status)),
            _ => None,
        })
        .filter(|(_, _, _, status)| matches!(status, StepStatus::Failed { .. }))
        .collect();

    assert_eq!(failure_events.len(), 1);
    assert!(matches!(
        &failure_events[0],
        (host, 2, description, StepStatus::Failed { message })
            if host == &HostName::new("feta") && description == "remote-b" && message == "boom"
    ));
}

#[tokio::test]
async fn remote_error_does_not_duplicate_failed_progress() {
    struct PanicResolver;

    #[async_trait::async_trait]
    impl StepResolver for PanicResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            panic!("local resolver should not be called after remote failure");
        }
    }

    let (cancel, tx) = setup();
    let mut rx = tx.subscribe();
    let plan =
        StepPlan::new(vec![Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop }]);
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 1,
                description: "remote".into(),
                status: StepStatus::Started,
            },
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 1,
                description: "remote".into(),
                status: StepStatus::Failed { message: "boom".into() },
            },
        ],
        wait_for_cancel: None,
        result: Err("boom".into()),
    }]);

    let result = run_step_plan_with_remote_executor(
        plan,
        1,
        HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &PanicResolver,
        &remote,
    )
    .await;

    assert_eq!(result, CommandValue::Error { message: "boom".into() });

    let failure_events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|event| matches!(event, DaemonEvent::CommandStepUpdate { status: StepStatus::Failed { .. }, .. }))
        .collect();
    assert_eq!(failure_events.len(), 1);
}

#[tokio::test]
async fn remote_progress_maps_to_global_step_indices() {
    let (cancel, tx) = setup();
    let mut rx = tx.subscribe();
    let resolver = TestResolver::new(vec![Ok(StepOutcome::Completed), Ok(StepOutcome::Completed)]);
    let plan = StepPlan::new(vec![
        make_step("local-a"),
        Step { description: "remote-a".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
        Step { description: "remote-b".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop },
        make_step("local-b"),
    ]);
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 2,
                description: "remote-a".into(),
                status: StepStatus::Started,
            },
            RemoteStepProgressUpdate {
                batch_step_index: 0,
                batch_step_count: 2,
                description: "remote-a".into(),
                status: StepStatus::Succeeded,
            },
            RemoteStepProgressUpdate {
                batch_step_index: 1,
                batch_step_count: 2,
                description: "remote-b".into(),
                status: StepStatus::Started,
            },
            RemoteStepProgressUpdate {
                batch_step_index: 1,
                batch_step_count: 2,
                description: "remote-b".into(),
                status: StepStatus::Succeeded,
            },
        ],
        wait_for_cancel: None,
        result: Ok(vec![StepOutcome::Completed, StepOutcome::Completed]),
    }]);

    let result = run_step_plan_with_remote_executor(
        plan,
        7,
        HostName::local(),
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
        ExecutionEnvironmentPath::new("/repo"),
        cancel,
        tx,
        &resolver,
        &remote,
    )
    .await;

    assert_eq!(result, CommandValue::Ok);

    let remote_indices: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            DaemonEvent::CommandStepUpdate { host, step_index, .. } if host == HostName::new("feta") => Some(step_index),
            _ => None,
        })
        .collect();
    assert_eq!(remote_indices, vec![1, 1, 2, 2]);
}

#[tokio::test]
async fn cancellation_while_remote_segment_active_cancels_remote_batch() {
    struct PanicResolver;

    #[async_trait::async_trait]
    impl StepResolver for PanicResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            panic!("local resolver should not run");
        }
    }

    let wait_for_cancel = Arc::new(Notify::new());
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![RemoteStepProgressUpdate {
            batch_step_index: 0,
            batch_step_count: 1,
            description: "remote".into(),
            status: StepStatus::Started,
        }],
        wait_for_cancel: Some(Arc::clone(&wait_for_cancel)),
        result: Ok(vec![StepOutcome::Completed]),
    }]);
    let (cancel, tx) = setup();
    let plan =
        StepPlan::new(vec![Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop }]);

    let cancel_clone = cancel.clone();
    let remote_clone = remote.clone();
    let task = tokio::spawn(async move {
        run_step_plan_with_remote_executor(
            plan,
            11,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            ExecutionEnvironmentPath::new("/repo"),
            cancel_clone,
            tx,
            &PanicResolver,
            &remote_clone,
        )
        .await
    });

    tokio::task::yield_now().await;
    cancel.cancel();

    let result = task.await.expect("join");
    assert_eq!(result, CommandValue::Cancelled);
    assert_eq!(remote.cancelled_commands().await, vec![11]);
}

#[tokio::test]
async fn cancellation_while_remote_segment_active_returns_cancelled_even_if_remote_batch_errors() {
    struct PanicResolver;

    #[async_trait::async_trait]
    impl StepResolver for PanicResolver {
        async fn resolve(&self, _desc: &str, _action: StepAction, _prior: &[StepOutcome]) -> Result<StepOutcome, String> {
            panic!("local resolver should not run");
        }
    }

    let wait_for_cancel = Arc::new(Notify::new());
    let remote = TestRemoteExecutor::new(vec![TestRemoteBatch {
        assert_host: HostName::new("feta"),
        progress: vec![RemoteStepProgressUpdate {
            batch_step_index: 0,
            batch_step_count: 1,
            description: "remote".into(),
            status: StepStatus::Started,
        }],
        wait_for_cancel: Some(Arc::clone(&wait_for_cancel)),
        result: Err("cancelled".into()),
    }]);
    let (cancel, tx) = setup();
    let mut rx = tx.subscribe();
    let plan =
        StepPlan::new(vec![Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop }]);

    let cancel_clone = cancel.clone();
    let remote_clone = remote.clone();
    let task = tokio::spawn(async move {
        run_step_plan_with_remote_executor(
            plan,
            12,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            ExecutionEnvironmentPath::new("/repo"),
            cancel_clone,
            tx,
            &PanicResolver,
            &remote_clone,
        )
        .await
    });

    tokio::task::yield_now().await;
    cancel.cancel();

    let result = task.await.expect("join");
    assert_eq!(result, CommandValue::Cancelled);
    assert_eq!(remote.cancelled_commands().await, vec![12]);

    let failure_events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|event| matches!(event, DaemonEvent::CommandStepUpdate { status: StepStatus::Failed { .. }, .. }))
        .collect();
    assert!(failure_events.is_empty());
}

#[tokio::test]
async fn cancellation_after_remote_cancel_timeout_returns_cancelled_without_waiting_forever() {
    struct StalledRemoteExecutor {
        started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl RemoteStepExecutor for StalledRemoteExecutor {
        async fn execute_batch(
            &self,
            _request: RemoteStepBatchRequest,
            _progress_sink: Arc<dyn RemoteStepProgressSink>,
        ) -> Result<Vec<StepOutcome>, String> {
            self.started.notify_waiters();
            std::future::pending().await
        }

        async fn cancel_active_batch(&self, _command_id: u64) -> Result<(), String> {
            Err("timed out waiting for remote cancel response".into())
        }
    }

    let (cancel, tx) = setup();
    let started = Arc::new(Notify::new());
    let remote = StalledRemoteExecutor { started: Arc::clone(&started) };
    let plan =
        StepPlan::new(vec![Step { description: "remote".into(), host: StepHost::Remote(HostName::new("feta")), action: StepAction::Noop }]);

    let cancel_clone = cancel.clone();
    let task = tokio::spawn(async move {
        run_step_plan_with_remote_executor(
            plan,
            13,
            HostName::local(),
            RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            ExecutionEnvironmentPath::new("/repo"),
            cancel_clone,
            tx,
            &TestResolver::new(vec![]),
            &remote,
        )
        .await
    });

    started.notified().await;
    cancel.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_millis(200), task)
        .await
        .expect("cancellation should not wait forever")
        .expect("task should join");
    assert_eq!(result, CommandValue::Cancelled);
}
