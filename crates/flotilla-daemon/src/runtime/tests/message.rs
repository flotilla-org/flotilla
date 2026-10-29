use super::*;
use flotilla_crew::reconcilers::terminal_session::TerminalObservation;
use flotilla_resources::TerminalOccupancy;

#[tokio::test]
async fn redelivery_refreshes_live_codex_crews_and_ignores_externally_managed_homes() {
    use std::os::unix::fs::PermissionsExt;

    // Owned Shared homes rotate on the host. Pushed and user-mounted homes
    // retain agent rotation, while explicit homes opt out. Exhaust all three
    // transport cases through the real host registry and environment manager.
    for transport in ["pushed", "shared", "user-mount"] {
        let shared = transport == "shared";
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
            std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
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
                        cloud_sandbox: None,
                        provider_address: None,
                        host_direct: None,
                        docker: Some(flotilla_resources::DockerEnvironmentSpec {
                            image_build_ref: None,
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

        // The guest process boundary records the protected files call; the registry
        // and redelivery loop are real and must select only the managed vessel.
        struct GuestCredentialRunner(PathBuf);
        #[async_trait]
        impl CommandRunner for GuestCredentialRunner {
            async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
                unreachable!()
            }
            async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
                unreachable!()
            }
            async fn exists(&self, _: &str, _: &[&str]) -> bool {
                false
            }
            async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
                assert_eq!(path, Path::new(flotilla_credentials::CONTAINER_CODEX_HOME).join("auth.json"));
                assert_eq!(mode, 0o400);
                fs::write(&self.0, content).unwrap();
                Ok(())
            }
        }
        let guest_auth = temp.path().join("guest-auth.json");
        let id = EnvironmentId::new("work");
        daemon
            .register_provisioned_environment(
                id.clone(),
                Arc::new(CodexTestEnvironment {
                    mounts: if transport == "pushed" {
                        Vec::new()
                    } else {
                        let mut mount = deliveries[0].mount.clone();
                        if transport == "user-mount" {
                            let source = temp.path().join("user-home");
                            fs::create_dir_all(&source).unwrap();
                            mount.host_path = DaemonHostPath::new(source);
                        }
                        vec![mount]
                    },
                    inner: TestInteriorEnvironment {
                        id,
                        image: ImageId::new("test"),
                        runner: Arc::new(GuestCredentialRunner(guest_auth.clone())),
                        env_vars: HashMap::new(),
                        destroyed: Arc::new(AtomicBool::new(false)),
                    },
                }),
                EnvironmentBag::new(),
                Some(passthrough_registry()),
            )
            .unwrap();

        fs::write(&central, "{\"tokens\":{\"access_token\":\"access-token-two\"}}").expect("rotate central auth");
        redeliver_codex_credentials(&state, NAMESPACE).await.expect("redeliver the rotated credential");

        if shared {
            assert!(!guest_auth.exists(), "mounted homes must rotate on the host without a guest push");
        } else {
            assert!(fs::read_to_string(&guest_auth).unwrap().contains("access-token-two"));
        }
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
    let outcome =
        deliver_and_confirm(&pool, adapters.get("codex").map(|a| &**a), "agent", "review wake", TerminalDeliveryReadiness::TurnBoundary)
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
        deliver_and_confirm(&*submit_pool, Some(&*adapter), "agent", "review wake", TerminalDeliveryReadiness::TurnBoundary).await
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let convoys = backend.using::<Convoy>(NAMESPACE);
    let convoy = convoys.create(&empty_meta("subject"), &ConvoySpec::builder().workflow_ref("workflow".into()).build()).await.unwrap();
    convoys
        .update_status("subject", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() })
        .await
        .unwrap();
    let inbox = flotilla_messaging::MessageInbox::new(backend.clone(), NAMESPACE);
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
    let convoy = convoys.get("subject").await.unwrap();
    convoys
        .update_status("subject", &convoy.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() })
        .await
        .unwrap();
    pool.inner.set_captured_screen("agent", include_str!("../../../../flotilla-agent/src/fixtures/codex-2927/scrolled-back.txt")).await;
    let task =
        tokio::spawn(
            async move { deliver_guarded_and_confirm(&*task_pool, Some(&*adapter), "agent", "wake", &inbox, &["turn".into()]).await },
        );
    assert_eq!(task.await.unwrap().unwrap(), TerminalDeliveryOutcome::BatchSuperseded);
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
    use flotilla_agent_connector::{AgentTransport, Delivery, Operation, TransportOutcome};
    use flotilla_messaging::{delivery_target, submission_ref};
    use flotilla_resources::MessageSubmission;
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
    let profile = build_local_profile(&daemon, &registry).await.expect("profile");
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
    let batch = Delivery {
        operation: Operation::NewTurn,
        target: delivery_target(&holder),
        submission: submission_ref(
            &MessageSubmission::builder()
                .batch_id("batch".into())
                .crew_id("crew".into())
                .session(ID.into())
                .started_at(Utc::now())
                .members(vec!["a".into(), "b".into(), "c".into()])
                .build(),
        ),
        text: "first\nsecond\nthird".into(),
    };
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
    assert_eq!(AgentTransport::submit(&runtime, &batch).await, TransportOutcome::Pending);
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
    let observation =
        AgentTransport::observe(&runtime, &delivery_target(&holder), Some(&batch.submission)).await.expect("pending observation");
    assert!(!observation.ready && !observation.working && observation.evidence.is_none());
    sessions.update_status(ID, &holder.metadata.resource_version, &original).await.expect("restore holder observation");
    let mut accepted = false;
    for _ in 0..30 {
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        if matches!(AgentTransport::poll(&runtime, &batch).await, TransportOutcome::Accepted { .. }) {
            accepted = true;
            break;
        }
    }
    assert!(accepted, "stable Working establishes acceptance");
    assert_eq!(pool.inner.delivered.lock().await.as_slice(), &[(ID.to_string(), batch.text.clone(), true)]);
    // With no retained task, a restart can observe evidence but never submit.
    assert!(matches!(AgentTransport::poll(&runtime, &batch).await, TransportOutcome::Unconfirmed { .. }));
    assert_eq!(pool.inner.delivered.lock().await.len(), 1);
    for name in &batch.submission.members {
        backend.using::<flotilla_resources::Message>(NAMESPACE).delete(name).await.expect("retire direct transport fixtures");
    }
    // Cross-host delivery waits until the sender authority's convoy is
    // replicated. The receiver's real inbox then submits all three records
    // through the same terminal-pool boundary exactly once.
    let sender_backend =
        ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("message-sender"));
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
    inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("await replication");
    assert_eq!(pool.inner.delivered.lock().await.len(), 1, "no turn without replicated convoy context");
    backend
        .replica_writer::<Convoy>(NodeId::new("message-sender"), NAMESPACE)
        .replace(&sender_backend.using::<Convoy>(NAMESPACE).list().await.expect("authority snapshot"), Utc::now())
        .await
        .expect("replicate authority");
    refresh_message_test_attention(&runtime, &backend, ID).await;
    inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("batch submission");
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        refresh_message_test_attention(&runtime, &backend, ID).await;
        inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("acceptance observation");
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
    held_pool_delivery_scenario("screen").await;
}

// #3028: an attributed Codex completion settles both the legacy held input
// and its durable Message representation without submitting another copy.
#[tokio::test(start_paused = true)]
async fn held_pool_delivery_accepts_following_codex_idle_hook() {
    held_pool_delivery_scenario("notify").await;
}

// Native top-level activity acknowledges input; subagent completion and Idle
// without Working leave both held-delivery representations held.
#[tokio::test(start_paused = true)]
async fn held_pool_delivery_accepts_native_codex_lifecycle_and_ignores_child_stop() {
    held_pool_delivery_scenario("native").await;
    held_pool_delivery_scenario("native-prompt-only").await;
}

async fn held_pool_delivery_scenario(mode: &str) {
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
    let profile = build_local_profile(&daemon, &registry).await.expect("profile");
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
    status.started_at = Some(Utc::now() - chrono::Duration::minutes(20));
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
        runtime.deliver_message("agent", (&spec).into(), "wake", TerminalDeliveryReadiness::Startup).await.expect("begin write"),
        TerminalDeliveryOutcome::Pending
    );
    tokio::task::yield_now().await;
    assert_eq!(
        runtime.deliver_message("agent", (&spec).into(), "wake", TerminalDeliveryReadiness::Startup).await.expect("ambiguous result"),
        TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)
    );
    let prepared = flotilla_crew::reconcilers::terminal_session::TerminalPrepared::MessageDeliveryUnconfirmed {
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
    inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("observe idle while held");
    let observed = sessions.get("agent").await.expect("idle holder");
    assert_eq!(observed.status.as_ref().expect("holder status").attention.as_ref().expect("attention").state, TerminalAttentionState::Idle);
    // Capturing a baseline is not evidence that input was consumed.
    refresh_message_test_attention(&runtime, &backend, "agent").await;
    inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("observe unchanged idle output");
    let messages = backend.using::<flotilla_resources::Message>(NAMESPACE);
    let held = messages.list().await.expect("held inbox").items;
    assert_eq!(held.len(), 1);
    assert!(!held[0].status.as_ref().expect("held status").phase.has_delivery_evidence());
    assert!(held[0].status.as_ref().expect("held status").submission.is_some());
    if mode != "screen" {
        let store = flotilla_agent::shared_in_memory_agent_state_store();
        let (_, parser) = flotilla_agent::parser_for_harness("codex").expect("parser");
        let events = if mode.starts_with("native") {
            vec![
                ("session-start", "SessionStart"),
                ("subagent-stop", "SubagentStop"),
                ("stop", "Stop"),
                ("user-prompt-submit", "UserPromptSubmit"),
                ("pre-tool-use", "PreToolUse"),
                ("post-tool-use", "PostToolUse"),
                ("stop", "Stop"),
            ]
        } else {
            vec![("notify", ""), ("notify", "")]
        };
        for (event, native) in events {
            if mode == "native-prompt-only" && matches!(event, "pre-tool-use" | "post-tool-use") {
                continue;
            }
            let payload = if event == "notify" {
                serde_json::json!({"type":"agent-turn-complete","thread-id":"held-thread","cwd":"/workspace"})
            } else {
                let mut payload = serde_json::json!({"session_id":"held-thread","turn_id":"turn-1","cwd":"/workspace",
                    "hook_event_name":native,"transcript_path":"/workspace/transcript.jsonl","stop_hook_active":false,"last_assistant_message":null});
                if event == "stop" {
                    payload["transcript_path"] = serde_json::Value::Null;
                }
                if event == "subagent-stop" {
                    payload["agent_id"] = "child".into();
                    payload["agent_type"] = "worker".into();
                }
                payload
            };
            let parsed = parser.parse_event(event, &serde_json::to_vec(&payload).unwrap()).expect("hook");
            tokio::time::advance(Duration::from_millis(1)).await;
            let response = crate::server::dispatch_request_with_state(
                &runtime.state.daemon,
                &store,
                3028,
                flotilla_protocol::Request::AgentHook {
                    event: flotilla_protocol::AgentHookEvent::builder()
                        .attachable_id(flotilla_agent::allocate_attachable_id())
                        .harness(flotilla_protocol::AgentHarness::Codex)
                        .event_type(parsed.event_type.clone())
                        .maybe_session_id(parsed.session_id.clone())
                        .maybe_transcript_path(parsed.transcript_path.clone())
                        .native_codex_hook(parsed.native_codex_hook)
                        .terminal(flotilla_protocol::AgentHookTerminalRef { namespace: NAMESPACE.into(), session_name: "agent".into() })
                        .build(),
                },
            )
            .await;
            let flotilla_protocol::Message::Response { response, .. } = response else { panic!("hook response") };
            assert!(matches!(*response, flotilla_protocol::ResponseResult::Ok { .. }));
            if mode.starts_with("native") && matches!(event, "session-start" | "subagent-stop" | "stop") {
                let holder = sessions.get("agent").await.expect("holder");
                if holder.status.as_ref().unwrap().last_hook_working_at.is_none() {
                    inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("observe held");
                    assert!(
                        !messages.get("held-turn").await.unwrap().status.unwrap().phase.has_delivery_evidence(),
                        "{event} must leave input held"
                    );
                    let mut legacy = holder.clone();
                    legacy.spec = spec.clone();
                    let outcome = reconciler.reconcile(
                        &legacy,
                        &flotilla_crew::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
                            obstruction: None,
                            attention: None,
                            occupancy: TerminalOccupancy::Vacant,
                            output_digest: None,
                        }),
                        Utc::now(),
                    );
                    let mut status = legacy.status.unwrap();
                    if let Some(patch) = outcome.patch {
                        patch.apply(&mut status);
                    }
                    assert!(status.degraded.is_some(), "{event} must leave legacy input held");
                }
            }
        }
        let receiver = sessions.get("agent").await.expect("attributed receiver");
        // The pre-migration representation retains its queued message spec;
        // the durable Message receiver above has already retired that field.
        let mut legacy_receiver = receiver.clone();
        legacy_receiver.spec = spec.clone();
        let current = legacy_receiver.status.as_ref().expect("status");
        let mut settled = current.clone();
        reconciler
            .reconcile(
                &legacy_receiver,
                &flotilla_crew::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
                    obstruction: None,
                    attention: None,
                    occupancy: TerminalOccupancy::Vacant,
                    output_digest: None,
                }),
                Utc::now(),
            )
            .patch
            .expect("idle hook accepts legacy hold")
            .apply(&mut settled);
        assert!(settled.degraded.is_none(), "completion resolves the legacy held delivery");
        assert_eq!(settled.delivered_message_id.as_deref(), Some("held-turn"));
        let agents = store.lock().expect("agents");
        assert_eq!(agents.list_agents().len(), 1);
        if mode.starts_with("native") {
            assert_eq!(agents.list_agents()[0].1.transcript_path.as_deref(), Some("/workspace/transcript.jsonl"));
        }
    } else {
        pool.observations.store(100, Ordering::SeqCst);
        refresh_message_test_attention(&runtime, &backend, "agent").await;
    }
    inbox.reconcile_delivery(&keystroke_router(&runtime.state), Utc::now()).await.expect("observe late acceptance evidence");
    let accepted = messages.list().await.expect("accepted inbox").items;
    assert!(accepted[0].status.as_ref().expect("receipt status").phase.has_delivery_evidence());
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn accepted_but_unconfirmed_delivery_is_never_retyped() {
    let pool = DeliveryProbePool::new(vec![ScreenActivity::Stable]);

    let outcome =
        deliver_and_confirm(&pool, None, "agent", "stuck handoff", TerminalDeliveryReadiness::Startup).await.expect("delivery outcome");

    assert_eq!(outcome, TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
    assert_eq!(pool.deliveries.load(Ordering::SeqCst), 1);
    assert_eq!(pool.retries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn terminal_delivery_lookup_deduplicates_same_message_and_replaces_a_different_one() {
    let deliveries = StdMutex::new(HashMap::new());
    let pending_task = tokio::spawn(async {
        std::future::pending::<()>().await;
        Ok(TerminalDeliveryOutcome::Confirmed)
    });
    deliveries.lock().expect("delivery lock").insert(
        "agent".to_string(),
        PendingTerminalDelivery { message_batch: None, message: "first".to_string(), task: pending_task, engine_turn: None },
    );

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
    deliveries.lock().expect("delivery lock").insert(
        "agent".to_string(),
        PendingTerminalDelivery { message_batch: None, message: "second".to_string(), task: completed_task, engine_turn: None },
    );
    let TerminalDeliveryLookup::Taken(completed) = lookup_terminal_delivery(&deliveries, "agent", "second") else {
        panic!("finished delivery should be drained")
    };
    assert_eq!(completed.task.await.expect("delivery task").expect("delivery result"), TerminalDeliveryOutcome::Confirmed);
}

// Three receiver-homed intents remain queued through controller refusals, then
// reach the real adapter/pool boundary once, in FIFO order. An ambiguous write
// resolves on later Working evidence (or operator closure) without duplication.
#[hegel::test]
fn message_batch_uses_one_pool_input_and_releases_a_late_receipt(tc: hegel::TestCase) {
    // Generate both settlement paths for a possibly written batch. Controller
    // refusals are covered by the separate cleat recovery lifecycle scenario.
    let operator_closes = tc.draw(hegel::generators::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("paused scenario runtime");
    runtime.block_on(run_message_batch_pool_case(operator_closes));
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
        deliveries.lock().expect("pending transport tasks").insert(
            "agent".into(),
            PendingTerminalDelivery { message_batch: Some("batch".into()), message: "framed input".into(), task, engine_turn: None },
        );
        for text in ["framed input", "legacy follow-up"] {
            assert!(matches!(lookup_terminal_delivery(&deliveries, "agent", text), TerminalDeliveryLookup::InFlight));
            assert_eq!(deliveries.lock().expect("pending transport tasks").len(), 1);
        }
        deliveries.lock().expect("pending transport tasks").remove("agent").expect("retained in-flight task").task.abort();
    }
}

// #3028: Codex only emits turn-complete (Idle). A following idle hook on
// the recorded receiver settles a held submission, without a Working hook.
#[tokio::test]
async fn codex_idle_hook_confirms_held_submission() {
    use flotilla_agent_connector::AgentTransport;
    use flotilla_messaging::{delivery_target, submission_ref};
    use flotilla_resources::MessageSubmission;
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"codex-hook-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let (daemon, _) = crew_daemon(config.clone()).await;
    let backend = daemon.resource_backend();
    create_credential_test_session(&backend, "codex-held", "hook", "hook-work", "contained-vessel").await;
    let runtime = TerminalControllerRuntime {
        state: Arc::new(ControllerRuntimeState::new(
            daemon,
            config,
            Arc::new(ProviderRegistry::new()),
            None,
            "host".into(),
            "host-direct".into(),
        )),
    };
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let holder = sessions.get("codex-held").await.expect("holder");
    let mut initial = holder.status.clone().expect("status");
    initial.session_id = Some("cleat-codex".into());
    let holder = sessions.update_status("codex-held", &holder.metadata.resource_version, &initial).await.expect("terminal process");
    let now = Utc::now();
    let submitted_at = now - chrono::Duration::seconds(2);
    let submission = MessageSubmission::builder()
        .batch_id("batch".into())
        .crew_id("crew".into())
        .session("cleat-codex".into())
        .started_at(submitted_at)
        .members(vec!["held".into()])
        .build();
    // Named boundaries include pre-submission/equal timestamps, old hooks,
    // screen Idle, another harness, and a replaced process/crew generation.
    for (offset, source, adapter, session, crew, expected) in [
        (-1, TerminalAttentionSource::Hook, "codex", "cleat-codex", "crew", false),
        (0, TerminalAttentionSource::Hook, "codex", "cleat-codex", "crew", false),
        (1, TerminalAttentionSource::Screen, "codex", "cleat-codex", "crew", false),
        (1, TerminalAttentionSource::Hook, "claude-code", "cleat-codex", "crew", false),
        (1, TerminalAttentionSource::Hook, "codex", "replacement", "crew", false),
        (1, TerminalAttentionSource::Hook, "codex", "cleat-codex", "replacement", false),
        (1, TerminalAttentionSource::Hook, "codex", "cleat-codex", "crew", true),
    ] {
        let current = sessions.get("codex-held").await.expect("current");
        let mut status = current.status.expect("status");
        status.phase = flotilla_resources::TerminalSessionPhase::Running;
        status.started_at = Some(submitted_at - chrono::Duration::seconds(1));
        status.session_id = Some(session.into());
        status.crew = Some(
            flotilla_resources::CrewSessionStatus::builder()
                .id(crew.into())
                .adapter(adapter.into())
                .stance("trusted-implicit".into())
                .build(),
        );
        status.attention = Some(flotilla_resources::TerminalAttention {
            state: TerminalAttentionState::Idle,
            source,
            as_of: submitted_at + chrono::Duration::seconds(offset),
        });
        sessions.update_status("codex-held", &current.metadata.resource_version, &status).await.expect("hook evidence");
        // Observe through the pre-update snapshot too: the runtime must consult
        // the durable receiver generation before trusting a newly arriving hook.
        let observed =
            AgentTransport::observe(&runtime, &delivery_target(&holder), Some(&submission_ref(&submission))).await.expect("observe hold");
        assert_eq!(observed.evidence.is_some(), expected, "{adapter} {source:?} offset {offset} session {session} crew {crew}");
        if session != submission.session || crew != submission.crew_id {
            assert!(!observed.ready && !observed.working, "replacement attention cannot confirm or release old input");
        }
    }
}

// #3408: a namespace resync must cost one delivery pass, independent of the
// number of active messages. Real inbox/store/watch collaborators keep this
// at the scheduling seam; absent receivers avoid terminal subprocesses.
#[tokio::test(start_paused = true)]
async fn message_resync_coalesces_active_messages() {
    async fn wait_for_passes(metrics: &flotilla_store::load_metrics::ControllerLoadMetrics, minimum: u64) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                // Paused time advances only after runnable tasks drain, so this
                // also lets finite watch/write follow-ups reach quiescence.
                tokio::time::sleep(Duration::from_millis(1)).await;
                if metrics.snapshot().reconcile.count() >= minimum {
                    return;
                }
            }
        })
        .await
        .expect("controller must complete the expected passes within one virtual second");
    }
    for n in [0, 1, 10, 100] {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-resync\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(temp.path().to_path_buf()));
        let daemon = InProcessDaemon::new(
            vec![],
            Arc::clone(&config),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            HostName::new("message-resync"),
            Arc::new(crate::peer::HostPeerOverlay::default()),
        )
        .await;
        let inbox = daemon.message_inbox(NAMESPACE).await;
        let messages = daemon.resource_backend().using::<flotilla_resources::Message>(NAMESPACE);
        for i in 0..n {
            inbox
                .accept(
                    &empty_meta(&format!("message-{i}")),
                    &flotilla_resources::MessageSpec::builder()
                        .sender("principal:test".into())
                        .receiver(format!("flotilla/missing/work/role-{i}"))
                        .relation(flotilla_resources::MessageRelation::System)
                        .body("wake".into())
                        .build(),
                    Utc::now(),
                )
                .await
                .expect("admit");
        }
        let state = Arc::new(ControllerRuntimeState::new(
            daemon,
            config,
            Arc::new(ProviderRegistry::new()),
            None,
            "host-test".into(),
            "host-direct-host-test".into(),
        ));
        let metrics = Arc::new(flotilla_store::load_metrics::ControllerLoadMetrics::default());
        let heartbeat = Arc::new(AtomicU64::new(0));
        let controller = super::super::message::MessageController { state, namespace: NAMESPACE.into() };
        let task = tokio::spawn(controller.run(Duration::from_secs(60), heartbeat, Arc::clone(&metrics)));
        wait_for_passes(&metrics, 1).await;
        let before = metrics.snapshot().reconcile.count();
        tokio::time::advance(Duration::from_secs(60)).await;
        wait_for_passes(&metrics, before + 1).await;
        let passes = metrics.snapshot().reconcile.count() - before;
        assert_eq!(passes, 1, "active={n}: resync must run one inbox pass");
        // New-message watches must still wake an idle inbox, including after
        // startup with no records. Admissions can interleave with passes;
        // liveness here complements the quiescent resync budget above.
        let before = metrics.snapshot().reconcile.count();
        for i in 0..n.max(1) {
            inbox
                .accept(
                    &empty_meta(&format!("new-message-{i}")),
                    &flotilla_resources::MessageSpec::builder()
                        .sender("principal:test".into())
                        .receiver(format!("flotilla/missing/work/new-role-{i}"))
                        .relation(flotilla_resources::MessageRelation::System)
                        .body("wake".into())
                        .build(),
                    Utc::now(),
                )
                .await
                .expect("new admission");
        }
        wait_for_passes(&metrics, before + 1).await;
        let passes = metrics.snapshot().reconcile.count() - before;
        assert!(passes > 0, "active={n}: new admissions must wake delivery");
        // Every interleaved admission must have reached a delivery pass, even
        // if its watch event arrived while a previous snapshot was in use.
        for i in 0..n.max(1) {
            let message = messages.get(&format!("new-message-{i}")).await.expect("new message");
            assert_eq!(message.status.expect("observed").reason.as_deref(), Some("receiver role has no current holder"));
        }
        assert!(!task.is_finished(), "inbox loop must remain live");
        task.abort();
    }
}

// Environment boundary fixture: retain the same interior runner while exposing
// the provider's realized mounts to the production environment manager.
struct CodexTestEnvironment {
    inner: TestInteriorEnvironment,
    mounts: Vec<ProvisionedMount>,
}
#[async_trait]
impl ProvisionedEnvironment for CodexTestEnvironment {
    flotilla_discovery_testkit::injected_environment_handle!();
    fn id(&self) -> &EnvironmentId {
        self.inner.id()
    }
    fn image(&self) -> &ImageId {
        self.inner.image()
    }
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        self.mounts.clone()
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        self.inner.runner()
    }
    async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
        self.inner.observe().await
    }
    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        self.inner.env_vars().await
    }
    async fn destroy(&self) -> Result<(), String> {
        self.inner.destroy().await
    }
}

// #3961: controller input from an attachment or terminal-response source cannot
// hold an idle crew indefinitely. Empty and abandoned composers recover in the
// same session with one delivery, saved draft attention, and no operator.
#[tokio::test(start_paused = true)]
async fn controller_input_detach_and_empty_or_abandoned_composer_delivers_without_operator() {
    use flotilla_resources::{Message, MessageRelation, MessageSpec, ROLE_LABEL, VESSEL_LABEL};
    for (attached, abandoned) in [(false, false), (true, false), (false, true), (true, true)] {
        let temp = TempDir::new().expect("cross-host Message fixture operation succeeds");
        fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-pool-test\"\n")
            .expect("cross-host Message fixture operation succeeds");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let (daemon, _) = crew_daemon(config.clone()).await;
        let backend = daemon.resource_backend();
        let mut probe = DeliveryProbePool::new(vec![ScreenActivity::Stable]);
        probe.submission_error = false;
        probe.working_after_delivery = true;
        probe.recent_controller_input.store(true, Ordering::SeqCst);
        probe.controller_attached.store(attached, Ordering::SeqCst);
        if abandoned {
            *probe.draft.lock().unwrap() = Some("--saved draft\nwith 'quotes' and \\".into());
        }

        probe.screens = vec!["› Ask Codex to do anything".into(); 1000];
        probe.screens.push("new tool output\n• Working (1s • esc to interrupt)\n› Ask Codex to do anything".into());
        let pool = Arc::new(probe);
        let runner = Arc::new(ControllerCleatRunner { probe: pool.clone(), recognizer_installed: AtomicBool::new(false) });
        let cleat = Arc::new(flotilla_terminal::cleat::CleatTerminalPool::new(runner, "cleat", &EnvironmentBag::new()));
        let mut registry = probe_local_provider_registry(&daemon, &config).await.expect("cross-host Message fixture operation succeeds");
        Arc::get_mut(&mut registry).expect("cross-host Message fixture operation succeeds").terminal_pools.insert(
            "message-probe",
            ProviderDescriptor::named(ProviderCategory::TerminalPool, "message-probe"),
            cleat,
        );
        let profile = build_local_profile(&daemon, &registry).await.expect("cross-host Message fixture operation succeeds");
        ensure_host_direct_environment_exists(&backend, NAMESPACE, &profile).await.expect("cross-host Message fixture operation succeeds");
        backend
            .using::<Convoy>(NAMESPACE)
            .create(&empty_meta("message-pool"), &ConvoySpec::builder().workflow_ref("test".into()).project_ref(NAMESPACE.into()).build())
            .await
            .expect("cross-host Message fixture operation succeeds");
        create_credential_test_session(&backend, "agent", "message-pool", "message-pool-work", &profile.host_direct_environment_name())
            .await;
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

        refresh_message_test_attention(&runtime, &backend, "agent").await;
        let inbox = runtime.state.daemon.message_inbox(NAMESPACE).await;
        inbox
            .accept(
                &empty_meta("held-turn"),
                &MessageSpec::builder()
                    .sender("system:turn-rules".into())
                    .receiver("flotilla/message-pool/work/coder".into())
                    .relation(MessageRelation::System)
                    .body("recover this turn".into())
                    .build(),
                Utc::now(),
            )
            .await
            .unwrap();
        let now = Utc::now();
        inbox.reconcile_delivery(&keystroke_router(&runtime.state), now).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        inbox.reconcile_delivery(&keystroke_router(&runtime.state), now + chrono::Duration::seconds(1)).await.unwrap();
        assert!(pool.inputs.lock().unwrap().is_empty(), "refusal writes no input");
        let waiting = backend.using::<Message>(NAMESPACE).get("held-turn").await.unwrap().status.unwrap();
        assert!(waiting.retry.is_none(), "controller input never consumes an attempt");
        // Detach (or a terminal-response source falls quiet), then observe the
        // idle empty composer. The unverifiable fence still needs a selector.
        pool.recent_controller_input.store(false, Ordering::SeqCst);
        pool.controller_attached.store(false, Ordering::SeqCst);
        refresh_message_test_attention_at(&runtime, &backend, "agent", now + chrono::Duration::seconds(12)).await;
        inbox.reconcile_delivery(&keystroke_router(&runtime.state), now + chrono::Duration::seconds(12)).await.unwrap();
        for tick in 0..40 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(200)).await;
            tokio::task::yield_now().await;
            inbox
                .reconcile_delivery(
                    &keystroke_router(&runtime.state),
                    now + chrono::Duration::seconds(12) + chrono::Duration::milliseconds(tick * 200),
                )
                .await
                .unwrap();
            if backend
                .using::<Message>(NAMESPACE)
                .get("held-turn")
                .await
                .unwrap()
                .status
                .as_ref()
                .is_some_and(|status| status.resolved_receiver.is_some())
            {
                break;
            }
        }
        let status = backend.using::<Message>(NAMESPACE).get("held-turn").await.unwrap().status.unwrap();
        assert_eq!(status.resolved_receiver.as_ref().expect("same-session receipt").session, "agent");
        assert_eq!(pool.inputs.lock().unwrap().len(), 1);
        assert_eq!(pool.lifecycle_changes.load(Ordering::SeqCst), 0, "no child launch or termination during recovery");
        assert_eq!(pool.retries.load(Ordering::SeqCst), 0, "no composer-clearing or harness restart");
        assert_eq!(sessions.get("agent").await.unwrap().status.unwrap().crew.unwrap().id, "original-crew");
        let saved = sessions.get("agent").await.unwrap().status.unwrap().stashed_drafts;
        assert_eq!(saved.len(), usize::from(abandoned));
        if abandoned {
            assert_eq!(saved[0], "--saved draft\nwith 'quotes' and \\");
        }
    }
}
