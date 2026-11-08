use super::*;

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
        engine_environments(&daemon);
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

        for (name, env) in
            [("work", BTreeMap::new()), ("external-home", BTreeMap::from([("CODEX_HOME".to_string(), "/image/codex".to_string())]))]
        {
            let spec = EnvironmentSpec {
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
                ..Default::default()
            };
            engine_environments(&daemon).put_ready(name, &spec, None, &format!("container-{name}"), Default::default());
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
        redeliver_codex_credentials(&state).await.expect("redeliver the rotated credential");

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
