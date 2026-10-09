use super::*;

pub(super) struct TransientTerminalPool {
    pub(super) inner: FakeTerminalPool,
    pub(super) available: AtomicBool,
}

pub(super) struct DeliveryProbePool {
    pub(super) inputs: StdMutex<Vec<String>>,
    pub(super) activities: Vec<ScreenActivity>,
    pub(super) observations: AtomicUsize,
    pub(super) deliveries: AtomicUsize,
    pub(super) retries: AtomicUsize,
    pub(super) screens: Vec<String>,
    pub(super) submission_error: bool,
}

impl DeliveryProbePool {
    pub(super) fn new(activities: Vec<ScreenActivity>) -> Self {
        assert!(!activities.is_empty(), "delivery probe needs at least one activity state");
        Self {
            inputs: StdMutex::new(Vec::new()),
            activities,
            observations: AtomicUsize::new(0),
            deliveries: AtomicUsize::new(0),
            retries: AtomicUsize::new(0),
            screens: Vec::new(),
            submission_error: false,
        }
    }
}

#[async_trait]
impl TerminalPool for DeliveryProbePool {
    fn tracks_session_liveness(&self) -> bool {
        true
    }

    async fn list_sessions(&self) -> Result<Vec<ProviderTerminalSession>, String> {
        let observation = self.observations.fetch_add(1, Ordering::SeqCst);
        let screen_activity = self.activities.get(observation).copied().unwrap_or_else(|| *self.activities.last().expect("activity state"));
        Ok(vec![ProviderTerminalSession {
            session_name: "agent".to_string(),
            status: TerminalStatus::Running,
            command: Some("claude".to_string()),
            working_directory: Some(ExecutionEnvironmentPath::new("/workspace")),
            screen_activity: Some(screen_activity),
        }])
    }

    async fn ensure_session(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
        _tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        Ok(())
    }

    fn attach_args(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(Vec::new())
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }

    async fn capture_screen(&self, _: &str) -> Result<Option<String>, String> {
        let index = self.observations.load(Ordering::SeqCst).saturating_sub(1);
        Ok(self.screens.get(index).or_else(|| self.screens.last()).cloned())
    }

    async fn deliver(&self, _session_name: &str, text: &str) -> Result<(), String> {
        self.inputs.lock().expect("captured input log").push(text.to_string());
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        if self.submission_error {
            Err("reply lost after PTY accepted input".into())
        } else {
            Ok(())
        }
    }

    async fn retry_delivery(&self, _session_name: &str, _text: &str) -> Result<(), String> {
        self.retries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// This fake stands in for the external PTY and harness: submission starts
// a working turn, and the scenario explicitly ends it at the composer.
pub(super) struct HooklessComposerPool {
    pub(super) inner: FakeTerminalPool,
    pub(super) submitted_screen: &'static str,
}

#[async_trait]
impl TerminalPool for HooklessComposerPool {
    async fn list_sessions(&self) -> Result<Vec<ProviderTerminalSession>, String> {
        self.inner.list_sessions().await
    }

    async fn capture_screen(&self, id: &str) -> Result<Option<String>, String> {
        self.inner.capture_screen(id).await
    }

    async fn deliver(&self, id: &str, text: &str) -> Result<(), String> {
        self.inner.deliver(id, text).await?;
        self.inner.set_captured_screen(id, self.submitted_screen).await;
        Ok(())
    }

    async fn ensure_session(
        &self,
        _: &str,
        _: &str,
        _: &ExecutionEnvironmentPath,
        _: &TerminalEnvVars,
        _: &[TerminalSessionTag],
    ) -> Result<(), String> {
        unreachable!("scenario starts with a running crew")
    }

    fn attach_args(
        &self,
        _: &str,
        _: &str,
        _: &ExecutionEnvironmentPath,
        _: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(Vec::new())
    }

    async fn kill_session(&self, _: &str) -> Result<(), String> {
        unreachable!("scenario never tears down its crew")
    }
}

pub(super) async fn refresh_message_test_attention(runtime: &TerminalControllerRuntime, backend: &ResourceBackend, name: &str) {
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let holder = sessions.get(name).await.expect("attention holder");
    let observation = runtime.observe_attention(name, &holder.spec).await.expect("terminal observation").expect("observed attention");
    flotilla_resources::apply_status_patch(
        &sessions,
        name,
        &flotilla_resources::TerminalSessionStatusPatch::Observe {
            attention: observation.attention,
            occupancy: observation.occupancy,
            output_digest: observation.output_digest,
            observed_at: Utc::now(),
        },
    )
    .await
    .expect("terminal controller attention projection");
}

pub(super) async fn run_message_batch_pool_case(operator_closes: bool) {
    use flotilla_discovery_testkit::fake_discovery;
    use flotilla_resources::{Message, MessageExpectation, MessagePhase, MessageRelation, MessageSpec, ROLE_LABEL, VESSEL_LABEL};

    use crate::testkits::server::spawn_in_memory_request_topology_stateful;
    let temp = TempDir::new().expect("cross-host Message fixture operation succeeds");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-pool-test\"\n")
        .expect("cross-host Message fixture operation succeeds");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let backend = daemon.resource_backend();
    let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
    probe.submission_error = true;
    probe.screens = vec!["› Ask Codex to do anything".into(); 1000];
    probe.screens.push("new tool output\n• Working (1s • esc to interrupt)\n› Ask Codex to do anything".into());
    let pool = Arc::new(probe);
    let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("cross-host Message fixture operation succeeds");
    Arc::get_mut(&mut registry).expect("cross-host Message fixture operation succeeds").terminal_pools.insert(
        "message-probe",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "message-probe"),
        pool.clone(),
    );
    let profile = build_local_profile(&daemon, &registry).expect("cross-host Message fixture operation succeeds");
    ensure_host_direct_environment_exists(&backend, NAMESPACE, &profile).await.expect("cross-host Message fixture operation succeeds");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("message-pool"), &ConvoySpec::builder().workflow_ref("test".into()).project_ref(NAMESPACE.into()).build())
        .await
        .expect("cross-host Message fixture operation succeeds");
    create_credential_test_session(&backend, "agent", "message-pool", "message-pool-work", &profile.host_direct_environment_name()).await;
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let terminal = sessions.get("agent").await.expect("cross-host Message fixture operation succeeds");
    let mut spec = terminal.spec.clone();
    spec.pool = "message-probe".into();
    let TerminalSessionSource::Agent { selector, .. } = &mut spec.source else { unreachable!() };
    selector.adapter = Some("codex".into());
    let mut metadata = InputMeta::from(&terminal.metadata);
    metadata.labels.insert(CONVOY_LABEL.into(), "message-pool".into());
    metadata.labels.insert(VESSEL_LABEL.into(), "work".into());
    metadata.labels.insert(ROLE_LABEL.into(), "coder".into());
    let terminal = sessions
        .update(&metadata, &terminal.metadata.resource_version, &spec)
        .await
        .expect("cross-host Message fixture operation succeeds");
    let mut status = terminal.status.clone().expect("cross-host Message fixture operation succeeds");
    status.session_id = Some("agent".into());
    status.crew = Some(flotilla_resources::CrewSessionStatus {
        id: "original-crew".into(),
        adapter: "codex".into(),
        model: None,
        stance: "trusted-implicit".into(),
    });
    sessions
        .update_status("agent", &terminal.metadata.resource_version, &status)
        .await
        .expect("cross-host Message fixture operation succeeds");
    let runtime = TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            daemon.clone(),
            config,
            registry,
            None,
            profile.host_id.clone(),
            profile.host_direct_environment_name(),
        )),
    };
    // The sender learns the receiver from replicated resources, then uses
    // ordinary ResourceApply across the real in-memory request router.
    async fn refresh_attention(runtime: &TerminalControllerRuntime, backend: &ResourceBackend) {
        let sessions = backend.using::<TerminalSession>(NAMESPACE);
        let holder = sessions.get("agent").await.expect("attention holder");
        let observation =
            runtime.observe_attention("agent", &holder.spec).await.expect("terminal observation").expect("observed attention");
        flotilla_resources::apply_status_patch(
            &sessions,
            "agent",
            &flotilla_resources::TerminalSessionStatusPatch::Observe {
                attention: observation.attention,
                occupancy: observation.occupancy,
                output_digest: observation.output_digest,
                observed_at: Utc::now(),
            },
        )
        .await
        .expect("terminal controller attention projection");
    }
    refresh_attention(&runtime, &backend).await;
    let sender_temp = TempDir::new().expect("cross-host Message fixture operation succeeds");
    fs::write(sender_temp.path().join("daemon.toml"), "machine_id = \"message-sender-test\"\n")
        .expect("cross-host Message fixture operation succeeds");
    let sender = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(sender_temp.path())),
        fake_discovery(false),
        HostName::new("message-sender"),
        ResourceBackend::InMemory(Default::default()),
    )
    .await;
    let topology = spawn_in_memory_request_topology_stateful(sender.clone(), daemon.clone())
        .await
        .expect("cross-host Message fixture operation succeeds");
    let sender_backend = sender.resource_backend();
    let now = Utc::now();
    let convoys = backend.using::<Convoy>(NAMESPACE).list().await.expect("cross-host Message fixture operation succeeds");
    sender_backend
        .replica_writer::<Convoy>(daemon.node_id().clone(), NAMESPACE)
        .replace(&convoys, now)
        .await
        .expect("cross-host Message fixture operation succeeds");
    let terminals = sessions.list().await.expect("cross-host Message fixture operation succeeds");
    sender_backend
        .replica_writer::<TerminalSession>(daemon.node_id().clone(), NAMESPACE)
        .replace(&terminals, now)
        .await
        .expect("cross-host Message fixture operation succeeds");
    let inbox = daemon.message_inbox(NAMESPACE).await;
    for index in 0..3 {
        let intent = MessageSpec::builder()
            .sender("system:turn-rules".into())
            .receiver(format!("{NAMESPACE}/message-pool/work/coder"))
            .relation(MessageRelation::System)
            .body(format!("payload-{index}"))
            .expectation(MessageExpectation::None)
            .build();
        let mut events = sender.subscribe();
        let command = topology
            .client
            .execute(
                Command::builder()
                    .action(CommandAction::ResourceApply {
                        namespace: NAMESPACE.into(),
                        document: serde_json::json!({"apiVersion":"flotilla.work/v1","kind":"Message",
                    "metadata":{"name":format!("message-{index}")},"spec":intent}),
                    })
                    .build(),
            )
            .await
            .expect("cross-host Message fixture operation succeeds");
        let result = wait_for_command_result(&mut events, command).await;
        assert!(matches!(result, CommandValue::ResourceObject(..)), "receiver admission failed: {result:?}");
    }
    assert!(
        sender_backend.using::<Message>(NAMESPACE).list().await.expect("cross-host Message fixture operation succeeds").items.is_empty(),
        "intent is homed only at the receiver"
    );
    assert_eq!(backend.using::<Message>(NAMESPACE).list().await.expect("cross-host Message fixture operation succeeds").items.len(), 3);
    inbox.reconcile_delivery(&runtime, now).await.expect("cross-host Message fixture operation succeeds");
    tokio::time::sleep(Duration::from_secs(6)).await;
    inbox.reconcile_delivery(&runtime, now + chrono::Duration::seconds(1)).await.expect("cross-host Message fixture operation succeeds");
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
    {
        let inputs = pool.inputs.lock().expect("cross-host Message fixture operation succeeds");
        assert_eq!(inputs.len(), 1);
        assert!(
            inputs[0].find("payload-0").expect("cross-host Message fixture operation succeeds")
                < inputs[0].find("payload-1").expect("cross-host Message fixture operation succeeds")
        );
        assert!(
            inputs[0].find("payload-1").expect("cross-host Message fixture operation succeeds")
                < inputs[0].find("payload-2").expect("cross-host Message fixture operation succeeds")
        );
    }
    assert_eq!(
        sessions
            .get("agent")
            .await
            .expect("cross-host Message fixture operation succeeds")
            .status
            .expect("cross-host Message fixture operation succeeds")
            .attention
            .expect("cross-host Message fixture operation succeeds")
            .state,
        TerminalAttentionState::Idle
    );
    if operator_closes {
        sender_backend
            .replica_writer::<Message>(daemon.node_id().clone(), NAMESPACE)
            .replace(&backend.using::<Message>(NAMESPACE).list().await.expect("held batch feed"), Utc::now())
            .await
            .expect("replicate batch home");
        let mut events = sender.subscribe();
        let id = topology
            .client
            .execute(
                Command::builder()
                    .action(CommandAction::MessageFailBatch {
                        namespace: NAMESPACE.into(),
                        name: "message-0".into(),
                        reason: "cancel held batch after inspecting original session".into(),
                    })
                    .build(),
            )
            .await
            .expect("route operator closure to batch home");
        assert!(matches!(wait_for_command_result(&mut events, id).await, CommandValue::Ok));
        inbox.reconcile_delivery(&runtime, now + chrono::Duration::seconds(2)).await.expect("reap explicitly closed transport");
        for message in backend.using::<Message>(NAMESPACE).list().await.expect("closed batch audit").items {
            let status = message.status.expect("status");
            assert_eq!(status.phase, MessagePhase::DeadLettered);
            assert!(status.submission.is_some());
            assert!(status.resolved_receiver.is_none());
        }
        assert!(runtime.state.terminal_deliveries.lock().expect("transport bookkeeping").is_empty());
        assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1, "operator closure does not type input");
        return;
    }
    pool.observations.store(1000, Ordering::SeqCst);
    refresh_attention(&runtime, &backend).await;
    inbox.reconcile_delivery(&runtime, now + chrono::Duration::seconds(2)).await.expect("cross-host Message fixture operation succeeds");
    inbox.reconcile_delivery(&runtime, now + chrono::Duration::seconds(4)).await.expect("cross-host Message fixture operation succeeds");
    for message in backend.using::<Message>(NAMESPACE).list().await.expect("cross-host Message fixture operation succeeds").items {
        let status = message.status.expect("cross-host Message fixture operation succeeds");
        // A receipt fulfills a Message with no reply expectation; the
        // following pass advances it to Satisfied without another input.
        assert_eq!(status.phase, MessagePhase::Satisfied);
        assert_eq!(status.resolved_receiver.expect("cross-host Message fixture operation succeeds").crew_id, "original-crew");
    }
    assert!(runtime.state.terminal_deliveries.lock().expect("cross-host Message fixture operation succeeds").is_empty());
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
}

impl TransientTerminalPool {
    pub(super) fn new() -> Self {
        Self { inner: FakeTerminalPool::new(), available: AtomicBool::new(true) }
    }

    pub(super) fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
    }
}

#[async_trait]
impl TerminalPool for TransientTerminalPool {
    fn tracks_session_liveness(&self) -> bool {
        true
    }

    async fn list_sessions(&self) -> Result<Vec<ProviderTerminalSession>, String> {
        if !self.available.load(Ordering::SeqCst) {
            return Err("terminal pool temporarily unavailable".to_string());
        }
        self.inner.list_sessions().await
    }

    async fn ensure_session(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        self.inner.ensure_session(session_name, command, cwd, env_vars, tags).await
    }

    async fn ensure_session_with_size(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
        initial_size: Option<TerminalSize>,
    ) -> Result<(), String> {
        self.inner.ensure_session_with_size(session_name, command, cwd, env_vars, tags, initial_size).await
    }

    fn attach_args(
        &self,
        session_name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        self.inner.attach_args(session_name, command, cwd, env_vars)
    }

    async fn kill_session(&self, session_name: &str) -> Result<(), String> {
        self.inner.kill_session(session_name).await
    }
}

pub(super) async fn daemon_with_transient_terminal_pool(
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
    pool: Arc<TransientTerminalPool>,
) -> Arc<InProcessDaemon> {
    let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(pool as Arc<dyn TerminalPool>));
    let daemon =
        InProcessDaemon::new_with_resource_backend(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy"), backend)
            .await;
    daemon
        .replace_local_environment_bag_for_test(
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                .with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
        )
        .expect("transient pool environment bag");
    daemon
}

use flotilla_discovery_testkit::InProcessDiscoveryExt;
