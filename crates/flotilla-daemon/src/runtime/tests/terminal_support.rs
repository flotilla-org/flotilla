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
    pub(super) lifecycle_changes: AtomicUsize,
    pub(super) retries: AtomicUsize,
    pub(super) screens: Vec<String>,
    pub(super) submission_error: bool,
    pub(super) working_after_delivery: bool,
    pub(super) recent_controller_input: AtomicBool,
    pub(super) controller_attached: AtomicBool,
    pub(super) draft: StdMutex<Option<String>>,
    pub(super) controller_refusals: AtomicUsize,
    pub(super) submission_attempts: AtomicUsize,
    pub(super) submission_attempted: tokio::sync::Notify,
}

impl DeliveryProbePool {
    pub(super) fn new(activities: Vec<ScreenActivity>) -> Self {
        assert!(!activities.is_empty(), "delivery probe needs at least one activity state");
        Self {
            inputs: StdMutex::new(Vec::new()),
            activities,
            observations: AtomicUsize::new(0),
            deliveries: AtomicUsize::new(0),
            lifecycle_changes: AtomicUsize::new(0),
            retries: AtomicUsize::new(0),
            screens: Vec::new(),
            submission_error: false,
            working_after_delivery: false,
            recent_controller_input: AtomicBool::new(false),
            controller_attached: AtomicBool::new(false),
            draft: StdMutex::new(None),
            controller_refusals: AtomicUsize::new(0),
            submission_attempts: AtomicUsize::new(0),
            submission_attempted: tokio::sync::Notify::new(),
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
        self.lifecycle_changes.fetch_add(1, Ordering::SeqCst);
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
        self.lifecycle_changes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn capture_screen(&self, _: &str) -> Result<Option<String>, String> {
        if self.working_after_delivery && self.deliveries.load(Ordering::SeqCst) > 0 {
            return Ok(Some("• Working (1s • esc to interrupt)".into()));
        }
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

    // Process boundary fake: the daemon guard refuses before any PTY bytes.
    async fn guarded_deliver(&self, session: &str, text: &str) -> Result<flotilla_terminal::TerminalSubmission, String> {
        self.submission_attempts.fetch_add(1, Ordering::SeqCst);
        self.submission_attempted.notify_one();
        if self.controller_refusals.try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1)).is_ok() {
            return Ok(flotilla_terminal::TerminalSubmission::Refused("controller input is not idle".into()));
        }
        self.deliver(session, text).await?;
        Ok(flotilla_terminal::TerminalSubmission::Submitted)
    }

    async fn operator_deliver(&self, session: &str, text: &str) -> Result<(), String> {
        self.deliver(session, text).await
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
    refresh_message_test_attention_at(runtime, backend, name, Utc::now()).await;
}

pub(super) async fn refresh_message_test_attention_at(
    runtime: &TerminalControllerRuntime,
    backend: &ResourceBackend,
    name: &str,
    now: chrono::DateTime<Utc>,
) {
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let holder = sessions.get(name).await.expect("attention holder");
    let mut observation =
        runtime.observe_attention(name, (&holder).into()).await.expect("terminal observation").expect("observed attention");
    if let Some(attention) = &mut observation.attention {
        attention.as_of = now;
    }
    flotilla_store::apply_status_patch(
        &sessions,
        name,
        &flotilla_resources::TerminalSessionStatusPatch::Observe {
            obstruction: None,
            attention: observation.attention,
            occupancy: observation.occupancy,
        },
    )
    .await
    .expect("terminal controller attention projection");
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
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        config,
        discovery,
        flotilla_protocol::HostName::new("dinghy"),
        backend,
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
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

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

// The vessel pool's process boundary exposes a Cleat endpoint; lifecycle behavior
// uses the shared in-memory terminal pool, with no subprocess or live host.
pub(super) struct AttachableToolPool(pub(super) FakeTerminalPool);

#[async_trait]
impl TerminalPool for AttachableToolPool {
    fn tracks_session_liveness(&self) -> bool {
        true
    }
    async fn list_sessions(&self) -> Result<Vec<ProviderTerminalSession>, String> {
        self.0.list_sessions().await
    }
    async fn ensure_session(
        &self,
        name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env: &TerminalEnvVars,
        tags: &[TerminalSessionTag],
    ) -> Result<(), String> {
        self.0.ensure_session(name, command, cwd, env, tags).await
    }
    async fn kill_session(&self, name: &str) -> Result<(), String> {
        self.0.kill_session(name).await
    }
    fn attach_args(
        &self,
        name: &str,
        command: &str,
        cwd: &ExecutionEnvironmentPath,
        env: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        self.0.attach_args(name, command, cwd, env)
    }
    async fn cleat_endpoint(&self, session: &str) -> Result<Option<flotilla_protocol::result_set::CleatEndpoint>, String> {
        Ok(Some(flotilla_protocol::result_set::CleatEndpoint {
            runtime_root: "/vessel/cleat".into(),
            daemon: "vessel-daemon".into(),
            session: session.into(),
        }))
    }
}

// Process-boundary stand-in for cleat: controller input closes an unverifiable
// fence, and only a matched composer selector plus quiet time can replace it.
// The scenario uses the real CleatTerminalPool parser and command construction.
pub(super) struct ControllerCleatRunner {
    pub(super) probe: Arc<DeliveryProbePool>,
    pub(super) recognizer_installed: AtomicBool,
}

#[async_trait]
impl CommandRunner for ControllerCleatRunner {
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        panic!("unexpected command discovery in delivery scenario")
    }

    async fn run(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &std::path::Path,
        label: &flotilla_provider_runtime::ChannelLabel,
    ) -> Result<String, String> {
        let output = self.run_output(cmd, args, cwd, label).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }

    async fn run_output(
        &self,
        _cmd: &str,
        args: &[&str],
        _cwd: &std::path::Path,
        _label: &flotilla_provider_runtime::ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        use flotilla_provider_runtime::CommandOutput;
        let start = args
            .iter()
            .position(|arg| matches!(*arg, "list" | "capture" | "inspect" | "recognizer" | "send" | "clear-composer"))
            .expect("cleat operation in controlled client envelope");
        let args = &args[start..];
        let quiet = !self.probe.recent_controller_input.load(Ordering::SeqCst);
        let refusal = |reason: &str| Ok(CommandOutput { stdout: String::new(), stderr: reason.into(), exit_code: Some(1) });
        let stdout = match args[0] {
            "list" => {
                self.probe.observations.fetch_add(1, Ordering::SeqCst);
                serde_json::json!([{"id":"agent","status":if self.probe.controller_attached.load(Ordering::SeqCst) {"Attached"} else {"Detached"}}]).to_string()
            }
            "capture" => self.probe.capture_screen("agent").await?.unwrap_or_default(),
            "inspect" => {
                let draft = self.probe.draft.lock().unwrap().clone().unwrap_or_default();
                let recognizer = self
                    .recognizer_installed
                    .load(Ordering::SeqCst)
                    .then(|| serde_json::json!({"id":"codex","error":null,"observation":{"roles":[{"role":"composer","value":draft}]}}));
                serde_json::json!({"last_controller_input_at": if quiet {0} else {Utc::now().timestamp_millis()}, "recognizer":recognizer})
                    .to_string()
            }
            "recognizer" => {
                assert_eq!(args, ["recognizer", "set", "agent", "codex"]);
                self.recognizer_installed.store(true, Ordering::SeqCst);
                String::new()
            }
            "clear-composer" => {
                let draft = args.iter().find_map(|arg| arg.strip_prefix("--draft=")).expect("exact draft guard, including leading hyphens");
                assert!(args.windows(2).any(|pair| pair == ["--controller-idle", "2m"]));
                if !quiet || self.probe.draft.lock().unwrap().as_deref() != Some(draft) {
                    return refusal("not-matched: composer changed before clear");
                }
                *self.probe.draft.lock().unwrap() = None;
                String::new()
            }
            "send" => {
                self.probe.submission_attempts.fetch_add(1, Ordering::SeqCst);
                if !quiet {
                    return refusal("controller-active: recent input (controller.cause=quiet-time)");
                }
                if !args.windows(2).any(|pair| pair == ["--if", "composer[value='']"])
                    || !args.windows(2).any(|pair| pair == ["--controller-idle", "2s"])
                {
                    return refusal("controller-active: unverifiable echo (controller.cause=fence-unverifiable)");
                }
                if self.probe.draft.lock().unwrap().is_some() {
                    return refusal("not-matched: composer holds a draft");
                }
                assert!(self.recognizer_installed.load(Ordering::SeqCst));
                self.probe.deliver("agent", args.last().unwrap()).await?;
                String::new()
            }
            other => panic!("unexpected cleat operation {other}"),
        };
        Ok(CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
    }
}
