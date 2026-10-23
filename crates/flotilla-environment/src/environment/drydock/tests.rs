use super::*;
use ::drydock::{client::Client, conformance};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput};
use hegel::generators as gs;

struct HostFixture {
    _directory: tempfile::TempDir,
    client: Client,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HostFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HostFixture {
    async fn start() -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("host.sock");
        let host = drydock_host::Host::new(drydock_host::reference::registry());
        let path = socket.clone();
        // SAFETY: geteuid has no preconditions; admit only this process's UID.
        let uid = unsafe { libc::geteuid() };
        let task = tokio::spawn(async move {
            host.serve_unix(path, uid, std::future::pending()).await.unwrap();
        });
        let client = Client::unix(socket).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while client.hello().await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let configuration = serde_json::from_value(serde_json::json!({
            "modules":[{"name":"memory","entrypoint":"reference"}],
            "providers":[{"name":"reference","module":"memory","settings":{"fixture":true},"credential":{"mode":"direct","credential_reference":"test"}}]
        })).unwrap();
        client.configure(&configuration).await.unwrap();
        Self { _directory: directory, client, task }
    }
    async fn ready(&self) {
        self.client.credential("reference", wire::Secret::new("reference-only-test-key")).await.unwrap();
    }
    fn provider(&self) -> DrydockEnvironmentProvider {
        let mut provider = DrydockEnvironmentProvider::new(
            Arc::new(self.client.clone()),
            "reference".into(),
            "reference".into(),
            self._directory.path().join("keys"),
        );
        provider.connector = Arc::new(ReferenceGuest);
        provider
    }
}
// The reference module has no guest. This fake stands only for the SSH/agent
// process boundary; every lifecycle call still runs through the real Rust host.
struct ReferenceGuest;
#[async_trait]
impl reach::Connector for ReferenceGuest {
    async fn connect(&self, _: &dyn ProviderApi, _: &str, _: &str, _: PathBuf) -> Result<Arc<dyn reach::Guest>, String> {
        Ok(Arc::new(Self))
    }
}
#[async_trait]
impl reach::Guest for ReferenceGuest {
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::new(Self)
    }
    async fn configure(&self, _: BTreeMap<String, String>) -> Result<(), String> {
        Ok(())
    }
    async fn deliver(&self, _: EnvironmentToolAsset) -> Result<(), String> {
        Ok(())
    }
}
#[async_trait]
impl CommandRunner for ReferenceGuest {
    async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        Ok(String::new())
    }
    async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        true
    }
    async fn run_to_file(&self, _: &str, _: &[&str], _: &Path, destination: &Path) -> Result<(), String> {
        tokio::fs::write(destination, b"reference checkpoint").await.map_err(error)
    }
}
pub(super) fn spec() -> flotilla_resources::EnvironmentSpec {
    flotilla_resources::EnvironmentSpec {
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            probe_needs: Default::default(),
            image: format!("example.test/crew@sha256:{}", "a".repeat(64)),
            size: flotilla_resources::CloudSandboxSize { cpus: 1, memory_bytes: 1024, disk_bytes: 1024 },
            lifetime_cap_seconds: None,
            env: Default::default(),
        }),
        ..Default::default()
    }
}
// The checked-in upstream pin may still be unpublished. The reference module
// does not execute tools: in that state assert the production refusal, then
// prepare its explicit fixture tools through the same Rust host API. Flotilla's
// production prepare always uses build_tools_digest and has no override seam.
async fn fixture_prepared(host: &HostFixture, provider: &DrydockEnvironmentProvider) -> PreparedEnvironment {
    if ::drydock::launch::build_tools_digest().is_ok() {
        return provider.prepare(&spec(), &Default::default()).await.unwrap();
    }
    assert!(provider.prepare(&spec(), &Default::default()).await.err().expect("unpublished tools refuse").contains("tools image"));
    let spec = spec().cloud_sandbox.unwrap();
    let prepared = host
        .client
        .prepare(
            "reference",
            &wire::PrepareRequest {
                platform: None,
                image_digest: spec.image.clone(),
                tools_digest: "reference-fixture-tools".into(),
                size: size(&spec.size),
            },
        )
        .await
        .unwrap();
    PreparedEnvironment::new(&provider.owner, Prepared { prepared, spec })
}
// #3620: missing credentials stay visible and not-ready; hello becomes ready
// after the credential seam delivers a value, without recreating the provider.
#[tokio::test]
async fn reference_readiness_tracks_credential_delivery() {
    let host = HostFixture::start().await;
    let provider = host.provider();
    assert!(provider.capabilities().material_modes.is_empty(), "unknown declarations cannot claim Shared");
    assert!(matches!(provider.detect_readiness().await, ProviderReadiness::MissingCredential { .. }));
    assert!(provider.prepare(&spec(), &Default::default()).await.is_err());
    host.ready().await;
    assert_eq!(provider.detect_readiness().await, ProviderReadiness::Ready);
    let declared = provider.declaration.read().unwrap().clone().expect("readiness loaded the real hello");
    assert_eq!(provider.capabilities().material_modes, capabilities(&declared.capabilities).material_modes);
    assert!(provider.capabilities().material_modes.contains(&MaterialMode::Pushed));
    assert!(!provider.capabilities().material_modes.contains(&MaterialMode::Shared));
    assert_eq!(provider.capabilities().egress_enforcement, EgressEnforcement::Advisory);
    assert_eq!(provider.capabilities().checkout_strategies, vec![CheckoutStrategy::CloneInVessel]);
}
// #3620 / ADR 0070: preparation is instance-owned, provision is idempotent by
// label, recovery preserves opaque backing IDs, and repeated destroy is safe.
#[tokio::test]
async fn reference_lifecycle_through_environment_provider() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    assert!(host.provider().provision(EnvironmentId::new("crew"), &prepared, Default::default()).await.is_err());
    let first = provider.provision(EnvironmentId::new("crew"), &prepared, Default::default()).await.unwrap();
    let second = provider.provision(EnvironmentId::new("crew"), &prepared, Default::default()).await.unwrap();
    assert_eq!(first.backing_id(), second.backing_id());
    let descriptor = first.provider_descriptor().unwrap();
    assert_eq!(descriptor.kind.implementation, "reference");
    assert!(!descriptor.host_version.is_empty() && !descriptor.module_version.is_empty());
    assert_eq!(descriptor.image_digest, spec().cloud_sandbox.unwrap().image);
    assert_eq!(first.local_image_id(), Some(format!("sha256:{}", "a".repeat(64)).as_str()));
    assert_eq!(
        provider.list_backings().await.unwrap(),
        vec![EnvironmentBacking { environment_id: EnvironmentId::new("crew"), backing_id: first.backing_id().into() }]
    );
    assert!(first.survivors().unwrap().host_checkpoint.is_some());
    let private = key_dir(&provider.directory, first.id());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o700);
    provider.destroy(first.backing_id()).await.unwrap();
    assert!(!private.join("id_ed25519").exists());
    assert!(private.join("checkpoint.tar").exists());
    provider.destroy(first.backing_id()).await.unwrap();
    assert!(provider.list_backings().await.unwrap().is_empty());
}
// The Drydock checker invokes adapter lifecycle methods via an injected HTTP
// executor. Streams retain the real host and preparation uses the published pin.
#[tokio::test]
async fn reference_passes_drydock_conformance() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = Arc::new(host.provider());
    let prepared = fixture_prepared(&host, &provider).await;
    let prepared = prepared.get::<Prepared>(&provider.owner).unwrap();
    let options = conformance::Options {
        request: wire::ProvisionRequest {
            label: "conformance".into(),
            prepared: prepared.prepared.clone(),
            size: size(&prepared.spec.size),
            egress: wire::EgressRequest::Open,
            ssh_public_key: None,
        },
        scenarios: vec![
            conformance::Scenario::Capabilities,
            conformance::Scenario::Prepare,
            conformance::Scenario::Lifecycle,
            conformance::Scenario::Observation,
            conformance::Scenario::Semantics,
            conformance::Scenario::Reach,
            conformance::Scenario::Refusal,
            conformance::Scenario::Destroy,
        ],
        fixture_configuration: None,
        null_environment_id: None,
        auth_audit: None,
        billing_sample_millis: 25,
        billing_settle_millis: 0,
        timeouts: Default::default(),
        echo_relay: true,
        reach_probe: None,
        egress_probe: None,
    };
    let adapter = Arc::new(super::conformance_test::AdapterHttp {
        provider: provider.clone(),
        prepared: Default::default(),
        handles: Default::default(),
    });
    let client = host.client.clone().with_http(adapter);
    let report = conformance::run(&client, "reference", &options).await;
    assert!(report.passed(), "{}", serde_json::to_string_pretty(&report).unwrap());
    assert!(provider.list_backings().await.unwrap().is_empty());
}
// ADR 0058: every terminal provider cause maps to Gone; a lifetime cap must
// never look resumable. Generate all causes and arbitrary process exit codes.
#[hegel::test]
fn terminal_observations_are_gone(tc: hegel::TestCase) {
    let code = tc.draw(gs::integers::<i32>());
    let causes = [
        wire::GoneCause::LifetimeCapReached,
        wire::GoneCause::EvictedByProvider,
        wire::GoneCause::HostOutOfMemory,
        wire::GoneCause::VmmExited { code },
        wire::GoneCause::GuestPoweredOff,
        wire::GoneCause::DestroyedAtProvider,
        wire::GoneCause::MissingOnList,
        wire::GoneCause::DestroyedByRequest,
    ];
    let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(causes.len() - 1));
    let mapped = observation(wire::Observation::Gone { cause: causes[index].clone() });
    assert!(matches!(mapped, ObservationState::Gone { .. }));
    assert_eq!(
        observation(wire::Observation::Gone { cause: wire::GoneCause::LifetimeCapReached }),
        ObservationState::Gone { cause: GoneCause::LifetimeCapReached }
    );
    assert!(matches!(
        observation(wire::Observation::Unknown { reason: wire::UnknownReason::HostUnreachable }),
        ObservationState::Unknown { .. }
    ));
}

struct NoTools;
#[async_trait]
impl ::drydock::ToolDetector for NoTools {
    async fn available(&self, _: &str) -> bool {
        false
    }
}
// #3620: a configured instance remains advertised as missing-tool when its
// managed language host cannot start. The real supervisor owns this detection.
#[tokio::test]
async fn missing_host_binary_is_not_ready() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let mut supervisor =
        ::drydock::Supervisor::new(directory.path().join("hosts"), Arc::new(::drydock::credentials::BuiltinCredentials::default()));
    supervisor.detector = Arc::new(NoTools);
    let host = Arc::new(
        supervisor
            .start(::drydock::HostSpec {
                name: "missing".into(),
                connection: ::drydock::HostConnection::Managed {
                    command: vec!["missing-language-host".into()],
                    runtime: Some("missing-runtime".into()),
                    proxy_environment: Default::default(),
                },
                configuration: wire::HostConfiguration { modules: vec![], providers: vec![] },
                credentials: Default::default(),
            })
            .await
            .unwrap(),
    );
    let provider = DrydockEnvironmentProvider::new(
        Arc::new(host.client.clone()),
        "configured".into(),
        "daytona".into(),
        directory.path().join("keys"),
    )
    .with_host(host);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(provider.detect_readiness().await, ProviderReadiness::MissingTool { .. }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

// Failed refreshes must neither replace the recovery floor nor leave a partial
// archive. Credential-bearing survivors remain private and explicitly retained.
#[tokio::test]
async fn failed_checkpoint_retains_last_archive_and_removes_partial() {
    struct Broken;
    #[async_trait]
    impl CommandRunner for Broken {
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            unreachable!()
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            unreachable!()
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
        async fn run_to_file(&self, _: &str, _: &[&str], _: &Path, destination: &Path) -> Result<(), String> {
            tokio::fs::write(destination, b"partial").await.unwrap();
            Err("connection lost".into())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("checkpoint.tar");
    tokio::fs::write(&target, b"last complete").await.unwrap();
    assert!(write_checkpoint(Arc::new(Broken), &target, Path::new("/work")).await.is_err());
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"last complete");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

// One slow SSH handshake must not block another label; repeated adoption must
// refresh the existing guest's credential environment without reconnecting it.
#[tokio::test]
async fn independent_provisions_and_rotated_tokens() {
    struct Guest {
        environments: Arc<std::sync::Mutex<Vec<BTreeMap<String, String>>>>,
    }
    #[async_trait]
    impl reach::Guest for Guest {
        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::new(ReferenceGuest)
        }
        async fn configure(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
            self.environments.lock().unwrap().push(environment);
            Ok(())
        }
        async fn deliver(&self, _: EnvironmentToolAsset) -> Result<(), String> {
            Ok(())
        }
    }
    struct Connector {
        calls: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        environments: Arc<std::sync::Mutex<Vec<BTreeMap<String, String>>>>,
    }
    #[async_trait]
    impl reach::Connector for Connector {
        async fn connect(&self, _: &dyn ProviderApi, _: &str, _: &str, _: PathBuf) -> Result<Arc<dyn reach::Guest>, String> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(Arc::new(Guest { environments: self.environments.clone() }))
        }
    }
    let host = HostFixture::start().await;
    host.ready().await;
    let mut provider = host.provider();
    let connector = Arc::new(Connector {
        calls: Default::default(),
        entered: Default::default(),
        release: Default::default(),
        environments: Default::default(),
    });
    provider.connector = connector.clone();
    let prepared = Arc::new(fixture_prepared(&host, &provider).await);
    let provider = Arc::new(provider);
    let task = {
        let provider = provider.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { provider.provision(EnvironmentId::new("slow"), &prepared, Default::default()).await })
    };
    connector.entered.notified().await;
    let fast = tokio::time::timeout(Duration::from_secs(3), provider.provision(EnvironmentId::new("fast"), &prepared, Default::default()))
        .await
        .unwrap()
        .unwrap();
    connector.release.notify_one();
    let slow = task.await.unwrap().unwrap();
    let opts = ProvisionOpts { tokens: vec![("TEST_TOKEN".into(), "rotated".into())], ..Default::default() };
    let adopted = provider.provision(EnvironmentId::new("fast"), &prepared, opts).await.unwrap();
    assert_eq!(adopted.backing_id(), fast.backing_id());
    assert_eq!(connector.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(connector.environments.lock().unwrap().last().unwrap().get("TEST_TOKEN").map(String::as_str), Some("rotated"));
    fast.destroy().await.unwrap();
    slow.destroy().await.unwrap();
}

// #3765 B1: after a controller restart, failed guest setup must leave the
// existing, work-carrying backing available for the next adoption attempt.
#[tokio::test]
async fn restart_failed_adoption_preserves_backing() {
    struct Unreachable;
    // Fake the SSH process boundary; provision and inventory use the real host.
    #[async_trait]
    impl reach::Connector for Unreachable {
        async fn connect(&self, _: &dyn ProviderApi, _: &str, _: &str, _: PathBuf) -> Result<Arc<dyn reach::Guest>, String> {
            Err("injected SSH failure".into())
        }
    }
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let first = provider.provision(EnvironmentId::new("survivor"), &prepared, Default::default()).await.unwrap();
    let backing = first.backing_id().to_owned();
    drop(first);
    drop(provider);
    let mut restarted = host.provider();
    restarted.connector = Arc::new(Unreachable);
    let prepared = fixture_prepared(&host, &restarted).await;
    assert!(restarted.provision(EnvironmentId::new("survivor"), &prepared, Default::default()).await.is_err());
    assert_eq!(host.client.inspect("reference", &backing).await.unwrap().id, backing);
}

// #3765 S3: after SSH closure, liveness reconnects and re-establishes the
// configured environment and forwards without recreating the provider.
#[tokio::test]
async fn closed_session_reconnects_on_liveness() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[derive(Default)]
    struct Recovery {
        closed: AtomicBool,
        connects: AtomicUsize,
        configurations: AtomicUsize,
        forwards: AtomicUsize,
    }
    // Fake only SSH/agent process execution; provider lifecycle uses the host.
    struct Guest(Arc<Recovery>);
    struct Connector(Arc<Recovery>);
    #[async_trait]
    impl reach::Guest for Guest {
        async fn closed(&self) -> bool {
            self.0.closed.load(Ordering::SeqCst)
        }
        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::new(ReferenceGuest)
        }
        async fn configure(&self, env: BTreeMap<String, String>) -> Result<(), String> {
            assert_eq!(env.get("TOKEN").map(String::as_str), Some("rotated"));
            self.0.configurations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn configure_lab(&self, lab: Option<(String, u16)>) -> Result<(), String> {
            assert_eq!(lab, Some(("forgejo.lab.flotilla.work".into(), 443)));
            self.0.forwards.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn deliver(&self, _: EnvironmentToolAsset) -> Result<(), String> {
            Ok(())
        }
    }
    #[async_trait]
    impl reach::Connector for Connector {
        async fn connect(&self, _: &dyn ProviderApi, _: &str, _: &str, _: PathBuf) -> Result<Arc<dyn reach::Guest>, String> {
            self.0.closed.store(false, Ordering::SeqCst);
            self.0.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Guest(self.0.clone())))
        }
    }
    let host = HostFixture::start().await;
    host.ready().await;
    let recovery = Arc::new(Recovery::default());
    let mut provider = host.provider().with_lab_reach(Some(("forgejo.lab.flotilla.work".into(), 443)));
    provider.connector = Arc::new(Connector(recovery.clone()));
    let prepared = fixture_prepared(&host, &provider).await;
    let opts = ProvisionOpts { tokens: [("TOKEN".into(), "rotated".into())].into_iter().collect(), ..Default::default() };
    let handle = provider.provision(EnvironmentId::new("recovery"), &prepared, opts).await.unwrap();
    let configured = recovery.configurations.load(Ordering::SeqCst);
    let forwarded = recovery.forwards.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(handle.observe().await.state, ObservationState::Running) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    recovery.closed.store(true, Ordering::SeqCst);
    let observation = handle.observe().await;
    assert!(matches!(observation.state, ObservationState::Running), "{observation:?}");
    assert_eq!(recovery.connects.load(Ordering::SeqCst), 2);
    assert_eq!(recovery.configurations.load(Ordering::SeqCst), configured + 1);
    assert_eq!(recovery.forwards.load(Ordering::SeqCst), forwarded + 1);
    handle.observe().await;
    assert_eq!(recovery.connects.load(Ordering::SeqCst), 2);
    handle.destroy().await.unwrap();
}

// #3765 F3: cancelling a streaming checkpoint removes partial host material
// while preserving the last complete survivor. Fake the remote stream boundary.
#[tokio::test]
async fn cancelled_checkpoint_removes_partial_material() {
    struct Stream(tokio::sync::Notify);
    #[async_trait]
    impl CommandRunner for Stream {
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            unreachable!()
        }
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            unreachable!()
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
        async fn run_to_file(&self, _: &str, _: &[&str], _: &Path, destination: &Path) -> Result<(), String> {
            tokio::fs::write(destination, b"partial credential material").await.unwrap();
            self.0.notify_one();
            std::future::pending().await
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("checkpoint.tar");
    tokio::fs::write(&target, b"complete").await.unwrap();
    let runner = Arc::new(Stream(Default::default()));
    let task = tokio::spawn({
        let runner = runner.clone();
        let target = target.clone();
        async move { write_checkpoint(runner, &target, Path::new("/workspace")).await }
    });
    runner.0.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read(target).unwrap(), b"complete");
}

// #3765 S2: build caches do not consume the survivor budget, and an oversized
// archive is refused before any bytes stream to the host. Use real GNU tools
// over a small temporary tree; sparse files cross the bound without disk cost.
#[tokio::test]
async fn checkpoint_excludes_caches_and_preflights_size() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("work ' quoted");
    tokio::fs::create_dir_all(root.join("target")).await.unwrap();
    tokio::fs::create_dir_all(root.join(".cargo/registry")).await.unwrap();
    tokio::fs::write(root.join("work.txt"), b"uncommitted work").await.unwrap();
    for path in [root.join("target/output"), root.join(".cargo/registry/cache")] {
        let file = std::fs::File::create(path).unwrap();
        file.set_len(1024 * 1024 * 1024).unwrap();
    }
    let runner = flotilla_provider_runtime::ProcessCommandRunner;
    let target = directory.path().join("archive.tar");
    let script = checkpoint_script(&[root.to_string_lossy().into_owned()]);
    runner.run_to_file("sh", &["-c", &script], Path::new("/"), &target).await.unwrap();
    let listing = runner.run("tar", &["-tf", target.to_str().unwrap()], Path::new("/"), &ChannelLabel::Default).await.unwrap();
    assert!(listing.contains("work.txt"));
    assert!(!listing.contains("target/output"));
    assert!(!listing.contains("registry/cache"));
    let oversized = std::fs::File::create(root.join("oversized-work")).unwrap();
    oversized.set_len(512 * 1024 * 1024 + 1).unwrap();
    let refused = directory.path().join("refused.tar");
    assert!(runner.run_to_file("sh", &["-c", &script], Path::new("/"), &refused).await.is_err());
    assert_eq!(std::fs::metadata(refused).unwrap().len(), 0);
}

// #3765 F4: either missing half makes a keypair unusable; repair both halves
// before retrying setup, while retaining the label-idempotent backing.
#[tokio::test]
async fn keypair_repair_requires_both_files() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    for (index, missing) in ["id_ed25519", "id_ed25519.pub"].into_iter().enumerate() {
        let id = EnvironmentId::new(format!("repair-{index}"));
        let first = provider.provision(id.clone(), &prepared, Default::default()).await.unwrap();
        let directory = key_dir(&provider.directory, &id);
        tokio::fs::remove_file(directory.join(missing)).await.unwrap();
        let repaired = provider.provision(id, &prepared, Default::default()).await.unwrap();
        assert_eq!(first.backing_id(), repaired.backing_id());
        assert!(directory.join("id_ed25519").is_file());
        assert!(directory.join("id_ed25519.pub").is_file());
        repaired.destroy().await.unwrap();
    }
}

// #3765 S2: periodic refresh failure preserves the completed archive and makes
// checkpoint staleness visible through health. Only the SSH/agent is faked.
#[tokio::test]
async fn checkpoint_refresh_failure_reports_stale_health() {
    #[derive(Clone, Default)]
    struct Guest(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl reach::Connector for Guest {
        async fn connect(&self, _: &dyn ProviderApi, _: &str, _: &str, _: PathBuf) -> Result<Arc<dyn reach::Guest>, String> {
            Ok(Arc::new(self.clone()))
        }
    }
    #[async_trait]
    impl reach::Guest for Guest {
        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::new(self.clone())
        }
        async fn configure(&self, _: BTreeMap<String, String>) -> Result<(), String> {
            Ok(())
        }
        async fn deliver(&self, _: EnvironmentToolAsset) -> Result<(), String> {
            Ok(())
        }
    }
    #[async_trait]
    impl CommandRunner for Guest {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            ReferenceGuest.run(cmd, args, cwd, label).await
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            ReferenceGuest.run_output(cmd, args, cwd, label).await
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            true
        }
        async fn run_to_file(&self, _: &str, _: &[&str], _: &Path, path: &Path) -> Result<(), String> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::fs::write(path, b"complete").await.map_err(error)
            } else {
                Err("checkpoint exceeds budget".into())
            }
        }
    }
    let host = HostFixture::start().await;
    host.ready().await;
    let mut provider = host.provider();
    provider.connector = Arc::new(Guest::default());
    let prepared = fixture_prepared(&host, &provider).await;
    let id = EnvironmentId::new("checkpoint-health");
    let handle = provider.provision(id.clone(), &prepared, Default::default()).await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(handle.detect_readiness().await, ProviderReadiness::Unknown { reason } if reason.contains("checkpoint stale")));
    assert_eq!(tokio::fs::read(key_dir(&provider.directory, &id).join("checkpoint.tar")).await.unwrap(), b"complete");
    tokio::time::resume();
    handle.destroy().await.unwrap();
}

// #3765 liveness: an inventory snapshot collected before creation must not
// override direct inspection of a live backing. Pin that interleaving explicitly.
#[tokio::test]
async fn stale_snapshot_absence_is_confirmed_by_inspect() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("snapshot-race"), &prepared, Default::default()).await.unwrap();
    let backing = host.client.inspect("reference", handle.backing_id()).await.unwrap();
    let adopted = provider.handle(backing, spec().cloud_sandbox.unwrap().image, BTreeMap::new()).unwrap();
    *adopted.watch.state.write().unwrap() = Some(ObservationState::Gone { cause: GoneCause::MissingOnList });
    assert!(matches!(adopted.observe().await.state, ObservationState::Running));
    handle.destroy().await.unwrap();
}
