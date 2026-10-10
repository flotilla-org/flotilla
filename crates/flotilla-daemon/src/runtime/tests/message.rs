use super::*;

#[tokio::test]
async fn redelivery_refreshes_live_codex_crews_and_ignores_externally_managed_homes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"codex-redelivery-test\"\n").expect("daemon config");
    let home = temp.path().join("home");
    let central = home.join(".config/flotilla/credentials/codex-central/auth.json");
    fs::create_dir_all(central.parent().expect("central credential directory")).expect("central credential directory");
    fs::write(&central, "{\"tokens\":{\"access_token\":\"access-token-one\"}}").expect("central auth");
    fs::set_permissions(&central, fs::Permissions::from_mode(0o600)).expect("protect central auth");
    let config = Arc::new(ConfigStore::with_base(config_base));
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
        HostName::new("dinghy"),
    )
    .await;
    let material = Arc::new(test_agent_material_registry(Arc::new(TestEnvVars::new([
        ("HOME", home.display().to_string()),
        (FLOTILLA_SKILLS_DIR_ENV, write_test_skill_sources(temp.path()).display().to_string()),
    ]))));
    let state = ControllerRuntimeState::new(
        Arc::clone(&daemon),
        config,
        Arc::new(ProviderRegistry::new()),
        None,
        "host-test".to_string(),
        "host-direct-host-test".to_string(),
    )
    .with_agent_material(Arc::clone(&material));
    let required = BTreeSet::from(["codex".to_string()]);
    let deliveries = material.prepare("work", &required, &BTreeMap::new()).await.expect("initial delivery");
    let codex_home = deliveries[0].mount.host_path.as_path().to_path_buf();

    let environments = daemon.resource_backend().using::<Environment>(NAMESPACE);
    for (name, env) in
        [("work", BTreeMap::new()), ("external-home", BTreeMap::from([("CODEX_HOME".to_string(), "/image/codex".to_string())]))]
    {
        let environment = environments
            .create(
                &empty_meta(name),
                &EnvironmentSpec {
                    host_direct: None,
                    docker: Some(flotilla_resources::DockerEnvironmentSpec {
                        image_composition: None,
                        image_build_ref: None,
                        memory_policy: Default::default(),
                        host_ref: "host-test".to_string(),
                        image: "test".to_string(),
                        declared_agent_adapters: required.clone(),
                        required_agent_adapters: required.clone(),
                        pull_policy: Default::default(),
                        mounts: Vec::new(),
                        env,
                    }),
                },
            )
            .await
            .expect("environment");
        environments
            .update_status(
                &environment.metadata.name,
                &environment.metadata.resource_version,
                &flotilla_resources::EnvironmentStatus { phase: EnvironmentPhase::Ready, ready: true, ..Default::default() },
            )
            .await
            .expect("ready");
    }

    fs::write(&central, "{\"tokens\":{\"access_token\":\"access-token-two\"}}").expect("rotate central auth");
    redeliver_codex_credentials(&state, NAMESPACE).await.expect("redeliver the rotated credential");

    let delivered = codex_home.join("auth.json");
    assert!(
        fs::read_to_string(&delivered).expect("delivered credential").contains("access-token-two"),
        "a live crew must be moved onto the refresher's current token"
    );
    assert_eq!(
        fs::metadata(&delivered).expect("delivered credential metadata").permissions().mode() & 0o777,
        0o400,
        "redelivery must keep the crew's copy read-only"
    );
    assert!(
        !home.join(".local/share/flotilla/agent-homes/external-home").exists(),
        "an externally managed CODEX_HOME is not ours to deliver into"
    );
}

// A permission prompt after submission proves the turn was accepted;
// retrying would interrupt that new turn and duplicate its text.
#[tokio::test(start_paused = true)]
async fn hookless_submission_needing_permission_is_confirmed_without_retry() {
    let pool = HooklessComposerPool {
        inner: FakeTerminalPool::new(),
        submitted_screen: "Would you like to run the following command?\n› 1. Yes\n  2. No",
    };
    pool.inner
        .add_sessions(vec![ProviderTerminalSession::builder()
            .session_name("agent".into())
            .status(TerminalStatus::Running)
            .command("codex".into())
            .screen_activity(ScreenActivity::Stable)
            .build()])
        .await;
    pool.inner.set_captured_screen("agent", "› Ask Codex to do anything").await;
    let adapters = AgentAdapterRegistry::discover(
        &EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex")),
        Arc::new(DiscoveryMockRunner::builder().build()),
    );
    let outcome = deliver_and_confirm(
        &pool,
        adapters.get("codex").map(|a| &**a),
        "agent",
        "review wake",
        TerminalDeliveryReadiness::TurnBoundary,
        false,
    )
    .await
    .expect("submit");
    assert_eq!(outcome, TerminalDeliveryOutcome::Confirmed);
    assert_eq!(pool.inner.delivered.lock().await.len(), 1);
}

// A new turn may retain its idle composer and active redraws briefly after
// submission. Working evidence within the grace window confirms it once.
#[tokio::test(start_paused = true)]
async fn hookless_submission_waits_for_delayed_working_evidence_with_composer_visible() {
    let pool = Arc::new(HooklessComposerPool { inner: FakeTerminalPool::new(), submitted_screen: "› Ask Codex to do anything" });
    pool.inner
        .add_sessions(vec![ProviderTerminalSession::builder()
            .session_name("agent".into())
            .status(TerminalStatus::Running)
            .screen_activity(ScreenActivity::Active)
            .build()])
        .await;
    pool.inner.set_captured_screen("agent", "› Ask Codex to do anything").await;
    let adapters = AgentAdapterRegistry::discover(
        &EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex")),
        Arc::new(DiscoveryMockRunner::builder().build()),
    );
    let adapter = adapters.get("codex").expect("adapter").clone();
    let submit_pool = pool.clone();
    let submit = tokio::spawn(async move {
        deliver_and_confirm(&*submit_pool, Some(&*adapter), "agent", "review wake", TerminalDeliveryReadiness::TurnBoundary, false).await
    });
    tokio::task::yield_now().await;
    assert_eq!(pool.inner.delivered.lock().await.len(), 1);
    tokio::time::advance(Duration::from_millis(1900)).await;
    assert!(!submit.is_finished());
    assert_eq!(pool.inner.delivered.lock().await.len(), 1);
    pool.inner.set_captured_screen("agent", "• Working (1s • esc to interrupt)\n› Ask Codex to do anything").await;
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(submit.await.expect("task").expect("delivery"), TerminalDeliveryOutcome::Confirmed);
    assert_eq!(pool.inner.delivered.lock().await.len(), 1);
}

// #2927: a captured scrollback composer permits input, while an unknown
// screen is held for at most five minutes and cannot paste on late release.
#[tokio::test(start_paused = true)]
async fn captured_scrollback_delivery_and_bounded_hold() {
    let adapters = AgentAdapterRegistry::discover(
        &EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex")),
        Arc::new(DiscoveryMockRunner::builder().build()),
    );
    for idle in [true, false] {
        let pool = Arc::new(HooklessComposerPool {
            inner: FakeTerminalPool::new(),
            submitted_screen: include_str!("../../../../flotilla-core/src/fixtures/codex-2927/working-after-release.txt"),
        });
        pool.inner
            .add_sessions(vec![ProviderTerminalSession::builder()
                .session_name("agent".into())
                .status(TerminalStatus::Running)
                .screen_activity(ScreenActivity::Stable)
                .build()])
            .await;
        pool.inner
            .set_captured_screen(
                "agent",
                if idle {
                    include_str!("../../../../flotilla-core/src/fixtures/codex-2927/scrolled-back.txt")
                } else {
                    "unobservable screen"
                },
            )
            .await;
        let backend = ResourceBackend::InMemory(Default::default());
        let inbox = flotilla_store::MessageInbox::new(backend, NAMESPACE);
        inbox
            .accept(
                &empty_meta("turn"),
                &flotilla_resources::MessageSpec::builder()
                    .sender("system:test".into())
                    .receiver("flotilla/coder".into())
                    .relation(flotilla_resources::MessageRelation::System)
                    .body("wake".into())
                    .build(),
                Utc::now(),
            )
            .await
            .unwrap();
        let task_pool = pool.clone();
        let adapter = adapters.get("codex").unwrap().clone();
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(async move {
            deliver_guarded_and_confirm(&*task_pool, Some(&*adapter), "agent", "wake", &inbox, &["turn".into()]).await
        });
        tokio::task::yield_now().await;
        if !idle {
            tokio::time::advance(Duration::from_secs(299)).await;
            tokio::task::yield_now().await;
            assert!(!task.is_finished());
            assert!(pool.inner.delivered.lock().await.is_empty());
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let outcome = task.await.unwrap().unwrap();
        if !idle {
            assert!(started.elapsed() <= Duration::from_secs(300), "held task must stop at the bound");
        }
        assert_eq!(
            outcome,
            if idle {
                TerminalDeliveryOutcome::Confirmed
            } else {
                TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)
            }
        );
        pool.inner.set_captured_screen("agent", include_str!("../../../../flotilla-core/src/fixtures/codex-2927/scrolled-back.txt")).await;
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(pool.inner.delivered.lock().await.len(), usize::from(idle));
    }
}

// Release must reject a firing condition that changed during a screen wait,
// even when the holder now has a genuine captured idle composer.
#[tokio::test(start_paused = true)]
async fn held_turn_is_revalidated_at_actual_input_boundary() {
    let adapters = AgentAdapterRegistry::discover(
        &EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex")),
        Arc::new(DiscoveryMockRunner::builder().build()),
    );
    let pool = Arc::new(HooklessComposerPool { inner: FakeTerminalPool::new(), submitted_screen: "• Working (1s • esc to interrupt)" });
    pool.inner
        .add_sessions(vec![ProviderTerminalSession::builder()
            .session_name("agent".into())
            .status(TerminalStatus::Running)
            .screen_activity(ScreenActivity::Stable)
            .build()])
        .await;
    pool.inner.set_captured_screen("agent", "unobservable screen").await;
    let backend = ResourceBackend::InMemory(Default::default());
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys.create(&empty_meta("subject"), &ConvoySpec::builder().workflow_ref("workflow".into()).build()).await.unwrap();
    convoys
        .update_status("subject", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() })
        .await
        .unwrap();
    let inbox = flotilla_store::MessageInbox::new(backend.clone(), NAMESPACE);
    inbox
        .accept(
            &empty_meta("turn"),
            &flotilla_resources::MessageSpec::builder()
                .sender("system:turn-rules".into())
                .receiver("flotilla/coder".into())
                .relation(flotilla_resources::MessageRelation::System)
                .body("wake".into())
                .delivery_condition("convoy/subject .status.phase == Active".parse().unwrap())
                .build(),
            Utc::now(),
        )
        .await
        .unwrap();
    let task_pool = pool.clone();
    let adapter = adapters.get("codex").unwrap().clone();
    let task =
        tokio::spawn(
            async move { deliver_guarded_and_confirm(&*task_pool, Some(&*adapter), "agent", "wake", &inbox, &["turn".into()]).await },
        );
    tokio::task::yield_now().await;
    let convoy = convoys.get("subject").await.unwrap();
    convoys
        .update_status("subject", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() })
        .await
        .unwrap();
    pool.inner.set_captured_screen("agent", include_str!("../../../../flotilla-core/src/fixtures/codex-2927/scrolled-back.txt")).await;
    assert_eq!(task.await.unwrap().unwrap(), TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady));
    assert!(pool.inner.delivered.lock().await.is_empty());
    assert_eq!(
        backend.using::<flotilla_resources::Message>(NAMESPACE).get("turn").await.unwrap().status.unwrap().phase,
        flotilla_resources::MessagePhase::Superseded
    );
}

// A fresh idle composer accepts one batch; stable Working supplies evidence.
// Once its task is gone, polling the durable intent never types it again.
#[tokio::test(start_paused = true)]
async fn durable_message_transport_accepts_idle_input_once_and_restart_poll_never_types() {
    use flotilla_resources::MessageSubmission;
    use flotilla_store::{MessageBatch, MessageTransport, MessageTransportOutcome};
    const ID: &str = "message-crew";
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-transport-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let backend = daemon.resource_backend();
    let pool = Arc::new(HooklessComposerPool {
        inner: FakeTerminalPool::new(),
        submitted_screen: "• Working (1s • esc to interrupt)\n› Ask Codex to do anything",
    });
    pool.inner
        .add_sessions(vec![ProviderTerminalSession::builder()
            .session_name(ID.into())
            .status(TerminalStatus::Running)
            .screen_activity(ScreenActivity::Stable)
            .build()])
        .await;
    pool.inner.set_captured_screen(ID, "› Ask Codex to do anything").await;
    let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("registry");
    Arc::get_mut(&mut registry).expect("exclusive registry").terminal_pools.insert(
        "hookless",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "hookless"),
        pool.clone(),
    );
    let profile = build_local_profile(&daemon, &registry).expect("profile");
    ensure_host_direct_environment_exists(&backend, NAMESPACE, &profile).await.expect("environment");
    create_credential_test_session(&backend, ID, "message", "message-work", &profile.host_direct_environment_name()).await;
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let holder = sessions.get(ID).await.expect("holder");
    let mut spec = holder.spec.clone();
    spec.pool = "hookless".into();
    let holder = sessions.update(&empty_meta(ID), &holder.metadata.resource_version, &spec).await.expect("pool");
    let runtime = TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            daemon,
            config,
            registry,
            None,
            profile.host_id.clone(),
            profile.host_direct_environment_name(),
        )),
    };
    let batch = MessageBatch::builder()
        .id("batch".into())
        .holder(holder)
        .submission(
            MessageSubmission::builder()
                .batch_id("batch".into())
                .crew_id("crew".into())
                .session(ID.into())
                .started_at(Utc::now())
                .members(vec!["a".into(), "b".into(), "c".into()])
                .build(),
        )
        .text("first\nsecond\nthird".into())
        .build();
    for name in &batch.submission.members {
        backend
            .using::<flotilla_resources::Message>(NAMESPACE)
            .create(
                &empty_meta(name),
                &flotilla_resources::MessageSpec::builder()
                    .sender("system:test".into())
                    .receiver("flotilla/coder".into())
                    .relation(flotilla_resources::MessageRelation::System)
                    .body("transport test".into())
                    .build(),
            )
            .await
            .expect("record batch member");
    }
    assert_eq!(MessageTransport::submit(&runtime, &batch).await, MessageTransportOutcome::Pending);
    // Unrelated hook/tool activity while the task owns input cannot invent
    // receipt, even with fresh Working evidence after submission began.
    let holder = sessions.get(ID).await.expect("pending holder");
    let original = holder.status.clone().expect("pending status");
    let mut status = original.clone();
    let activity_at = batch.submission.started_at + chrono::Duration::seconds(1);
    status.session_id = Some(ID.into());
    status.attention = Some(flotilla_resources::TerminalAttention {
        state: TerminalAttentionState::Working,
        as_of: activity_at,
        source: TerminalAttentionSource::Hook,
    });
    status.last_tool_activity_at = Some(activity_at);
    let holder = sessions.update_status(ID, &holder.metadata.resource_version, &status).await.expect("unrelated activity");
    let observation = MessageTransport::observe(&runtime, &holder, Some(&batch.submission)).await.expect("pending observation");
    assert!(!observation.ready && !observation.working && observation.evidence.is_none());
    sessions.update_status(ID, &holder.metadata.resource_version, &original).await.expect("restore holder observation");
    let mut accepted = false;
    for _ in 0..30 {
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        if matches!(MessageTransport::poll(&runtime, &batch).await, MessageTransportOutcome::Accepted { .. }) {
            accepted = true;
            break;
        }
    }
    assert!(accepted, "stable Working establishes acceptance");
    assert_eq!(pool.inner.delivered.lock().await.as_slice(), &[(ID.to_string(), batch.text.clone(), true)]);
    // With no retained task, a restart can observe evidence but never submit.
    assert!(matches!(MessageTransport::poll(&runtime, &batch).await, MessageTransportOutcome::Unconfirmed { .. }));
    assert_eq!(pool.inner.delivered.lock().await.len(), 1);
    for name in &batch.submission.members {
        backend.using::<flotilla_resources::Message>(NAMESPACE).delete(name).await.expect("retire direct transport fixtures");
    }
    // Cross-host delivery waits until the sender authority's convoy is
    // replicated. The receiver's real inbox then submits all three records
    // through the same terminal-pool boundary exactly once.
    let sender_backend = ResourceBackend::InMemory(Default::default()).with_local_root(NodeId::new("message-sender"));
    sender_backend
        .using::<Convoy>(NAMESPACE)
        .create(
            &empty_meta("message"),
            &flotilla_resources::ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build(),
        )
        .await
        .expect("sender authority");
    let current = sessions.get(ID).await.expect("current holder");
    let mut meta = InputMeta::from(&current.metadata);
    meta.labels.extend([
        (flotilla_resources::CONVOY_LABEL.into(), "message".into()),
        (flotilla_resources::VESSEL_LABEL.into(), "work".into()),
        (flotilla_resources::ROLE_LABEL.into(), "coder".into()),
    ]);
    let current = sessions.update(&meta, &current.metadata.resource_version, &current.spec).await.expect("holder address");
    let mut status = current.status.clone().expect("holder status");
    status.session_id = Some(ID.into());
    status.crew =
        Some(flotilla_resources::CrewSessionStatus { id: "crew".into(), adapter: "codex".into(), model: None, stance: "work".into() });
    sessions.update_status(ID, &current.metadata.resource_version, &status).await.expect("holder identity");
    pool.inner.set_captured_screen(ID, "› Ask Codex to do anything").await;
    let inbox = runtime.state.daemon.message_inbox(NAMESPACE).await;
    for index in 0..3 {
        let intent = flotilla_resources::MessageSpec::builder()
            .sender("flotilla/test".into())
            .receiver("flotilla/message/work/coder".into())
            .relation(flotilla_resources::MessageRelation::System)
            .body(format!("replicated-{index}"))
            .build();
        inbox.accept(&empty_meta(&format!("replicated-{index}")), &intent, Utc::now()).await.expect("receiver-home intent");
    }
    inbox.reconcile_delivery(&runtime, Utc::now()).await.expect("await replication");
    assert_eq!(pool.inner.delivered.lock().await.len(), 1, "no turn without replicated convoy context");
    backend
        .replica_writer::<Convoy>(NodeId::new("message-sender"), NAMESPACE)
        .replace(&sender_backend.using::<Convoy>(NAMESPACE).list().await.expect("authority snapshot"), Utc::now())
        .await
        .expect("replicate authority");
    refresh_message_test_attention(&runtime, &backend, ID).await;
    inbox.reconcile_delivery(&runtime, Utc::now()).await.expect("batch submission");
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        refresh_message_test_attention(&runtime, &backend, ID).await;
        inbox.reconcile_delivery(&runtime, Utc::now()).await.expect("acceptance observation");
        if backend
            .using::<flotilla_resources::Message>(NAMESPACE)
            .list()
            .await
            .expect("inbox")
            .items
            .iter()
            .all(|message| message.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence()))
        {
            break;
        }
    }
    let records = backend.using::<flotilla_resources::Message>(NAMESPACE).list().await.expect("receipts").items;
    assert_eq!(records.len(), 3);
    assert!(records.iter().all(|message| message
        .status
        .as_ref()
        .is_some_and(|status| status.phase.has_delivery_evidence()
            && status.resolved_receiver.as_ref().is_some_and(|receiver| receiver.session == ID))));
    let typed = pool.inner.delivered.lock().await;
    assert_eq!(typed.len(), 2, "one additional input for three replicated messages");
    assert!(typed[1].1.find("replicated-0").expect("first body") < typed[1].1.find("replicated-1").expect("second body"));
    assert!(typed[1].1.find("replicated-1").expect("first body") < typed[1].1.find("replicated-2").expect("second body"));
}

// #2599: all senders share prompt readiness, FIFO order, and exact receipts.
// Explicit-clock in-memory scenarios cover both cleat attachment states,
// stable/active/missing pixel metadata, and history exposed by attach resize.
#[tokio::test(start_paused = true)]
async fn hookless_composer_delivers_review_and_supervisor_turns_with_or_without_principal() {
    use flotilla_resources::{CrewMessageDelivery, CrewMessageSender, TerminalCrewMessage};
    const ID: &str = "hookless-crew";
    const COMPOSER: &str = "› Earlier submitted brief\nWorked for 10m 43s\n› Ask Codex to do anything\ngpt-6.1-sol · /workspace";
    for attached in [false, true] {
        for activity in [Some(ScreenActivity::Stable), Some(ScreenActivity::Active), None] {
            let temp = TempDir::new().expect("tempdir");
            fs::write(temp.path().join("daemon.toml"), "machine_id = \"hookless-test\"\n").expect("daemon config");
            let config = Arc::new(ConfigStore::with_base(temp.path()));
            let (daemon, _) = crew_daemon(config.clone()).await;
            let backend = daemon.resource_backend();
            let pool = Arc::new(HooklessComposerPool {
                inner: FakeTerminalPool::new(),
                submitted_screen: "• Working (1s • esc to interrupt)\n› Ask Codex to do anything",
            });
            pool.inner
                .add_sessions(vec![ProviderTerminalSession::builder()
                    .session_name(ID.into())
                    .status(if attached { TerminalStatus::Running } else { TerminalStatus::Disconnected })
                    .command("codex".into())
                    .working_directory(ExecutionEnvironmentPath::new("/workspace"))
                    .maybe_screen_activity(activity)
                    .build()])
                .await;
            let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("registry");
            Arc::get_mut(&mut registry).expect("exclusive test registry").terminal_pools.insert(
                "hookless",
                ProviderDescriptor::named(ProviderCategory::TerminalPool, "hookless"),
                pool.clone(),
            );
            let profile = build_local_profile(&daemon, &registry).expect("profile");
            ensure_host_direct_environment_exists(&backend, NAMESPACE, &profile).await.expect("environment");
            backend
                .using::<Convoy>(NAMESPACE)
                .create(&empty_meta("hookless"), &ConvoySpec::builder().workflow_ref("test".into()).role("hookless".into()).build())
                .await
                .expect("convoy");
            create_credential_test_session(&backend, ID, "hookless", "hookless-work", &profile.host_direct_environment_name()).await;
            let sessions = backend.using::<TerminalSession>(NAMESPACE);
            let mut session = sessions.get(ID).await.expect("session");
            let mut spec = session.spec.clone();
            spec.pool = "hookless".into();
            let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { unreachable!() };
            let make_message = |id: &str, sender| TerminalCrewMessage {
                id: id.into(),
                text: id.into(),
                sender,
                delivery: CrewMessageDelivery::Queued,
                acknowledged: Default::default(),
                following: Vec::new(),
            };
            let review = make_message("review-round-3", CrewMessageSender::FlotillaTurn { source: "review".into() });
            *message = Some(review.clone());
            let head = message.as_mut().expect("head");
            head.append(review); // Duplicate wakes must not create a second turn.
            head.append(make_message("supervisor-resume", CrewMessageSender::Governor { name: "porthole".into() }));
            head.append(make_message("operator-brief", CrewMessageSender::OperatorResume { principal: None }));
            let mut meta = InputMeta::from(&session.metadata);
            meta.labels.extend([
                (flotilla_resources::CONVOY_LABEL.into(), "hookless".into()),
                (flotilla_resources::VESSEL_LABEL.into(), "work".into()),
                (flotilla_resources::ROLE_LABEL.into(), "coder".into()),
            ]);
            let mut stored_spec = spec.clone();
            if let TerminalSessionSource::Agent { message, .. } = &mut stored_spec.source {
                *message = None;
            }
            session = sessions.update(&meta, &session.metadata.resource_version, &stored_spec).await.expect("new-only holder");
            let mut status = session.status.clone().expect("status");
            status.crew = Some(flotilla_resources::CrewSessionStatus {
                id: "crew-id".into(),
                adapter: "codex".into(),
                model: None,
                stance: "work".into(),
            });
            status.session_id = Some(ID.into());
            status.delivered_message_id = Some("initial-brief".into());
            sessions.update_status(ID, &session.metadata.resource_version, &status).await.expect("running status");
            let runtime = Arc::new(TerminalControllerRuntime {
                state: Arc::new(ControllerRuntimeState::new(
                    daemon,
                    config,
                    registry,
                    None,
                    profile.host_id.clone(),
                    profile.host_direct_environment_name(),
                )),
            });
            // Upgrade keeps the three distinct producer identities but
            // Message owns one batch and one evidence-backed receipt per record.
            let inbox = runtime.state.daemon.message_inbox(NAMESPACE).await;
            let TerminalSessionSource::Agent { message: Some(head), .. } = &spec.source else { unreachable!() };
            for input in std::iter::once(head).chain(head.following.iter()) {
                let intent = flotilla_resources::legacy_message_spec("flotilla/hookless/work/coder", input);
                let name = flotilla_resources::message_record_name(&intent.receiver, &intent.sender, &input.id);
                inbox.accept(&empty_meta(&name), &intent, Utc::now()).await.expect("new Message intent");
            }
            let inbox = runtime.state.daemon.message_inbox(NAMESPACE).await;
            for screen in [
                "• Working (10m • esc to interrupt)\n› Ask Codex to do anything",
                "Would you like to run the following command?\n› 1. Yes\n  2. No",
                "Unknown screen",
                "› please wait for my draft\n\ngpt-6.1-sol · /workspace",
                "› Ask Codex to do anything after I finish typing",
                "› Ask Codex to do anything\n  but wait for my second line\n\ngpt-6.1-sol · /workspace",
            ] {
                pool.inner.set_captured_screen(ID, screen).await;
                refresh_message_test_attention(&runtime, &backend, ID).await;
                inbox.reconcile_delivery(&*runtime, Utc::now()).await.expect("non-boundary observation");
                tokio::time::advance(Duration::from_millis(200)).await;
                tokio::task::yield_now().await;
                assert!(pool.inner.delivered.lock().await.is_empty(), "active turns and drafts must retain all input");
                let observed = sessions.get(ID).await.expect("receiver attention after observation");
                assert_ne!(
                    observed.status.expect("receiver status").attention.expect("refreshed attention").state,
                    TerminalAttentionState::Idle
                );
            }
            pool.inner.set_captured_screen(ID, COMPOSER).await;
            for _ in 0..40 {
                tokio::time::advance(Duration::from_millis(200)).await;
                tokio::task::yield_now().await;
                refresh_message_test_attention(&runtime, &backend, ID).await;
                inbox.reconcile_delivery(&*runtime, Utc::now()).await.expect("delivery observation");
                if backend
                    .using::<flotilla_resources::Message>(NAMESPACE)
                    .list()
                    .await
                    .expect("receiver delivery receipts")
                    .items
                    .iter()
                    .all(|message| message.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence()))
                {
                    break;
                }
            }
            let records = backend.using::<flotilla_resources::Message>(NAMESPACE).list().await.expect("adopted receiver inbox").items;
            assert_eq!(records.len(), 3);
            assert!(records.iter().all(|message| message.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence())));
            pool.inner.set_captured_screen(ID, COMPOSER).await;
            inbox.reconcile_delivery(&*runtime, Utc::now()).await.expect("repeat receipt");
            let delivered = pool.inner.delivered.lock().await;
            assert_eq!(delivered.len(), 1, "all three upgraded inputs share one turn");
            let text = &delivered[0].1;
            assert!(text.find("review-round-3").expect("first body") < text.find("supervisor-resume").expect("second body"));
            assert!(text.find("supervisor-resume").expect("first body") < text.find("operator-brief").expect("second body"));
            assert!(delivered[0].2);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn delivery_waits_for_the_agent_tui_to_become_idle_before_submitting() {
    let pool = DeliveryProbePool::new(vec![ScreenActivity::Active, ScreenActivity::Stable, ScreenActivity::Active]);

    let outcome = deliver_and_confirm(&pool, None, "agent", "first line\n\nsecond line", TerminalDeliveryReadiness::Startup, false)
        .await
        .expect("delivery outcome");

    assert_eq!(outcome, TerminalDeliveryOutcome::Confirmed);
    assert!(pool.observations.load(Ordering::SeqCst) >= 3);
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
}

// #2705: generate isolated redraws and short Working/Idle runs through
// the real Codex adapter and terminal-pool boundary. No flicker permits a
// second write; only a full stable second confirms the queued submission.
#[hegel::test]
fn flickering_codex_confirmation_never_resends(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let run_length = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let accepted_then_error = tc.draw(gs::booleans());
    let mut pool = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
    pool.submission_error = accepted_then_error;
    pool.screens.push("› Ask Codex to do anything".into());
    for index in 0..24 {
        pool.screens.push(if (index / run_length).is_multiple_of(2) {
            "• Working (10m • esc to interrupt)\n› Ask Codex to do anything".into()
        } else {
            "› Ask Codex to do anything".into()
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
    runtime.block_on(async {
        let adapters = AgentAdapterRegistry::discover(
            &EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex")),
            Arc::new(DiscoveryMockRunner::builder().build()),
        );
        let outcome = deliver_and_confirm(
            &pool,
            adapters.get("codex").map(|adapter| &**adapter),
            "agent",
            "wake",
            TerminalDeliveryReadiness::Startup,
            false,
        )
        .await
        .expect("uncertain submission");
        assert_eq!(outcome, TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
        assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
    });
}

// #2705: a successful pool write may already be queued by the agent even
// when screen attention never confirms submission. Do not type it again.
// #2755: the real terminal-pool/adapter and reconciler seams continue
// observing a held write, acknowledge later output, and never retype it.
#[tokio::test(start_paused = true)]
async fn held_pool_delivery_observes_late_consumption_without_resending() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"held-pool-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let backend = daemon.resource_backend();
    let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
    probe.submission_error = true; // External PTY accepts input but loses its reply.
    probe.screens = vec!["› Ask Codex to do anything".into(); 100];
    probe.screens.push("new tool output\n• Working (1s • esc to interrupt)\n› Ask Codex to do anything".into());
    let pool = Arc::new(probe);
    let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("registry");
    Arc::get_mut(&mut registry).expect("exclusive registry").terminal_pools.insert(
        "held-probe",
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "held-probe"),
        pool.clone(),
    );
    let profile = build_local_profile(&daemon, &registry).expect("profile");
    ensure_host_direct_environment_exists(&backend, NAMESPACE, &profile).await.expect("environment");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(&empty_meta("held-pool"), &ConvoySpec::builder().workflow_ref("test".into()).build())
        .await
        .expect("convoy");
    create_credential_test_session(&backend, "agent", "held-pool", "held-pool-work", &profile.host_direct_environment_name()).await;
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let mut session = sessions.get("agent").await.expect("session");
    let mut spec = session.spec.clone();
    spec.pool = "held-probe".into();
    let TerminalSessionSource::Agent { message, selector, .. } = &mut spec.source else { unreachable!() };
    selector.adapter = Some("codex".into());
    *message = Some(flotilla_resources::TerminalCrewMessage {
        id: "held-turn".into(),
        text: "wake".into(),
        sender: Default::default(),
        delivery: Default::default(),
        acknowledged: Default::default(),
        following: Vec::new(),
    });
    let mut meta = InputMeta::from(&session.metadata);
    meta.labels.extend([
        (flotilla_resources::CONVOY_LABEL.into(), "held-pool".into()),
        (flotilla_resources::VESSEL_LABEL.into(), "work".into()),
        (flotilla_resources::ROLE_LABEL.into(), "coder".into()),
    ]);
    let mut stored_spec = spec.clone();
    if let TerminalSessionSource::Agent { message, .. } = &mut stored_spec.source {
        *message = None;
    }
    session = sessions.update(&meta, &session.metadata.resource_version, &stored_spec).await.expect("new-only holder");
    let mut status = session.status.clone().expect("status");
    status.session_id = Some("agent".into());
    status.crew =
        Some(flotilla_resources::CrewSessionStatus { id: "held-crew".into(), adapter: "codex".into(), model: None, stance: "work".into() });
    session = sessions.update_status("agent", &session.metadata.resource_version, &status).await.expect("running session");
    let runtime = Arc::new(TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            daemon,
            config,
            registry,
            None,
            profile.host_id.clone(),
            profile.host_direct_environment_name(),
        )),
    });
    let reconciler = TerminalSessionReconciler::new(runtime.clone(), backend.clone(), NAMESPACE);
    assert_eq!(
        runtime.deliver_message("agent", &spec, "wake", TerminalDeliveryReadiness::Startup).await.expect("begin write"),
        TerminalDeliveryOutcome::Pending
    );
    tokio::task::yield_now().await;
    assert_eq!(
        runtime.deliver_message("agent", &spec, "wake", TerminalDeliveryReadiness::Startup).await.expect("ambiguous result"),
        TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)
    );
    let prepared = flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::MessageDeliveryUnconfirmed {
        message_id: "held-turn".into(),
        message: "accepted but reply lost".into(),
    };
    reconciler.reconcile(&session, &prepared, Utc::now()).patch.expect("hold").apply(&mut status);
    sessions.update_status("agent", &session.metadata.resource_version, &status).await.expect("persist hold");
    // The old held path observed a baseline before the upgrade. Preserve
    // that real observation so changed output has a comparison witness.
    refresh_message_test_attention(&runtime, &backend, "agent").await;
    session = sessions.get("agent").await.expect("legacy held baseline");
    let inbox = runtime.state.daemon.message_inbox(NAMESPACE).await;
    let intent = flotilla_resources::MessageSpec::builder()
        .sender("system:test".into())
        .receiver("flotilla/held-pool/work/coder".into())
        .relation(flotilla_resources::MessageRelation::System)
        .body("wake".into())
        .build();
    inbox.accept(&empty_meta("held-turn"), &intent, Utc::now()).await.expect("new Message intent");
    let messages = backend.using::<flotilla_resources::Message>(NAMESPACE);
    let record = messages.get("held-turn").await.expect("held message");
    let mut held = record.status.clone().expect("message status");
    held.submission = Some(
        flotilla_resources::MessageSubmission::builder()
            .batch_id("held-turn".into())
            .crew_id("held-crew".into())
            .session("agent".into())
            .started_at(Utc::now())
            .members(vec!["held-turn".into()])
            .maybe_output_digest(session.status.as_ref().and_then(|status| status.last_output_digest.clone()))
            .build(),
    );
    messages.update_status("held-turn", &record.metadata.resource_version, &held).await.expect("submission evidence");
    let inbox = runtime.state.daemon.message_inbox(NAMESPACE).await;
    refresh_message_test_attention(&runtime, &backend, "agent").await;
    inbox.reconcile_delivery(&*runtime, Utc::now()).await.expect("observe idle while held");
    let observed = sessions.get("agent").await.expect("idle holder");
    assert_eq!(observed.status.as_ref().expect("holder status").attention.as_ref().expect("attention").state, TerminalAttentionState::Idle);
    // Capturing a baseline is not evidence that input was consumed.
    refresh_message_test_attention(&runtime, &backend, "agent").await;
    inbox.reconcile_delivery(&*runtime, Utc::now()).await.expect("observe unchanged idle output");
    let messages = backend.using::<flotilla_resources::Message>(NAMESPACE);
    let held = messages.list().await.expect("held inbox").items;
    assert_eq!(held.len(), 1);
    assert!(!held[0].status.as_ref().expect("held status").phase.has_delivery_evidence());
    assert!(held[0].status.as_ref().expect("held status").submission.is_some());
    pool.observations.store(100, Ordering::SeqCst);
    refresh_message_test_attention(&runtime, &backend, "agent").await;
    inbox.reconcile_delivery(&*runtime, Utc::now()).await.expect("observe late output");
    let accepted = messages.list().await.expect("accepted inbox").items;
    assert!(accepted[0].status.as_ref().expect("receipt status").phase.has_delivery_evidence());
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn accepted_but_unconfirmed_delivery_is_never_retyped() {
    let pool = DeliveryProbePool::new(vec![ScreenActivity::Stable]);

    let outcome = deliver_and_confirm(&pool, None, "agent", "stuck handoff", TerminalDeliveryReadiness::Startup, false)
        .await
        .expect("delivery outcome");

    assert_eq!(outcome, TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn delivery_flags_a_session_that_never_becomes_idle_without_sending_bytes() {
    let pool = DeliveryProbePool::new(vec![ScreenActivity::Active]);

    let outcome = deliver_and_confirm(&pool, None, "agent", "unsafe handoff", TerminalDeliveryReadiness::Startup, false)
        .await
        .expect("delivery outcome");

    assert_eq!(outcome, TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady));
    assert_eq!(pool.observations.load(Ordering::SeqCst), DELIVERY_READY_POLLS);
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 0);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn delivery_to_an_established_busy_agent_waits_past_the_startup_deadline() {
    let mut activities = vec![ScreenActivity::Active; DELIVERY_READY_POLLS + 10];
    activities.extend([ScreenActivity::Stable, ScreenActivity::Active]);
    let pool = DeliveryProbePool::new(activities);

    let outcome = deliver_and_confirm(&pool, None, "agent", "handoff after this turn", TerminalDeliveryReadiness::TurnBoundary, false)
        .await
        .expect("delivery outcome");

    assert_eq!(outcome, TerminalDeliveryOutcome::Confirmed);
    assert!(pool.observations.load(Ordering::SeqCst) > DELIVERY_READY_POLLS);
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn replacement_delivery_clears_the_composer_before_submitting() {
    let pool = DeliveryProbePool::new(vec![ScreenActivity::Stable, ScreenActivity::Active]);

    let outcome = deliver_and_confirm(&pool, None, "agent", "replacement handoff", TerminalDeliveryReadiness::TurnBoundary, true)
        .await
        .expect("delivery outcome");

    assert_eq!(outcome, TerminalDeliveryOutcome::Confirmed);
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 0);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn terminal_delivery_lookup_deduplicates_same_message_and_replaces_a_different_one() {
    let deliveries = StdMutex::new(HashMap::new());
    let pending_task = tokio::spawn(async {
        std::future::pending::<()>().await;
        Ok(TerminalDeliveryOutcome::Confirmed)
    });
    deliveries
        .lock()
        .expect("delivery lock")
        .insert("agent".to_string(), PendingTerminalDelivery { message_batch: None, message: "first".to_string(), task: pending_task });

    assert!(matches!(lookup_terminal_delivery(&deliveries, "agent", "first"), TerminalDeliveryLookup::InFlight));
    let TerminalDeliveryLookup::Taken(replaced) = lookup_terminal_delivery(&deliveries, "agent", "second") else {
        panic!("different message should replace the in-flight delivery")
    };
    assert_eq!(replaced.message, "first");
    replaced.task.abort();
    assert!(matches!(lookup_terminal_delivery(&deliveries, "agent", "second"), TerminalDeliveryLookup::Vacant));

    let completed_task = tokio::spawn(async { Ok(TerminalDeliveryOutcome::Confirmed) });
    while !completed_task.is_finished() {
        tokio::task::yield_now().await;
    }
    deliveries
        .lock()
        .expect("delivery lock")
        .insert("agent".to_string(), PendingTerminalDelivery { message_batch: None, message: "second".to_string(), task: completed_task });
    let TerminalDeliveryLookup::Taken(completed) = lookup_terminal_delivery(&deliveries, "agent", "second") else {
        panic!("finished delivery should be drained")
    };
    assert_eq!(completed.task.await.expect("delivery task").expect("delivery result"), TerminalDeliveryOutcome::Confirmed);
}

// Three receiver-homed intents reach the real adapter/pool boundary as one
// ordered frame. An ambiguous write keeps observations fresh, resolves on
// later Working evidence, and releases its slot without typing again.
#[tokio::test(start_paused = true)]
async fn message_batch_uses_one_pool_input_and_releases_a_late_receipt() {
    for operator_closes in [false, true] {
        run_message_batch_pool_case(operator_closes).await;
    }
}

// Legacy queue work cannot drain or replace a Message batch's task,
// whether it is still pending or has finished awaiting its durable receipt.
#[tokio::test]
async fn message_batch_owns_its_task_until_the_receiver_releases_it() {
    for completed in [false, true] {
        let deliveries = StdMutex::new(HashMap::new());
        let task = tokio::spawn(async move {
            if !completed {
                std::future::pending::<()>().await;
            }
            Ok(TerminalDeliveryOutcome::Confirmed)
        });
        if completed {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        }
        deliveries
            .lock()
            .expect("pending transport tasks")
            .insert("agent".into(), PendingTerminalDelivery { message_batch: Some("batch".into()), message: "framed input".into(), task });
        for text in ["framed input", "legacy follow-up"] {
            assert!(matches!(lookup_terminal_delivery(&deliveries, "agent", text), TerminalDeliveryLookup::InFlight));
            assert_eq!(deliveries.lock().expect("pending transport tasks").len(), 1);
        }
        deliveries.lock().expect("pending transport tasks").remove("agent").expect("retained in-flight task").task.abort();
    }
}
