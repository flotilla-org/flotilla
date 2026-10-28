use super::*;
use ::drydock::{
    agent::{stand_in::Gateway, SshDialer},
    client::Client,
    conformance,
};
use hegel::generators as gs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The agent build the stand-in vessel runs and the provider pushes.
const BUILD: &[u8] = b"flotilla drydock test agent";

/// A real Rust drydock host with the reference module, whose vessels declare
/// an agent reached over SSH, and an in-process gateway serving a real agent
/// under that reach's forwarding rules. The stand-in vessel is this host: its
/// workspace root is the fixture's.
struct HostFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    runtime: ::drydock::runtime::Runtime,
    client: Client,
    gateway: Arc<Gateway>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HostFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HostFixture {
    async fn start() -> Self {
        Self::with_gateway(false).await
    }
    /// `reverse_forwards`: whether the provider's SSH server accepts them.
    async fn with_gateway(reverse_forwards: bool) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = directory.path().to_path_buf();
        let socket = root.join("host.sock");
        // The main server's runtime behind drydock's public router, as an
        // embedding or socket-backed caller sees it.
        let runtime = ::drydock::runtime::Runtime::new(drydock_host::reference::registry());
        let router = ::drydock::api::router(Arc::new(runtime.clone()), runtime.stopping());
        let listener = rigging_http_serve::UnixServer::bind(&socket, 0o600).await.unwrap();
        let task = tokio::spawn(async move {
            listener.serve(router, rigging_http_serve::Caller::<()>::Unix, std::future::pending()).await.unwrap();
        });
        let client = Client::unix(socket).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while client.hello().await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let reach = serde_json::json!({"kind": "ssh", "endpoint": "gateway.test:22", "user": "minted-gateway-user",
            "host_key": "ssh-ed25519 AAAA", "reverse_forwards": reverse_forwards});
        let configuration = serde_json::from_value(serde_json::json!({
            "modules":[{"name":"memory","entrypoint":"reference"}],
            "providers":[{"name":"reference","module":"memory","settings":{"fixture":true,"agent":true,"reach":reach,
                "workspace_root":root.join("workspace")},"credential":{"mode":"direct","credential_reference":"test"}}]
        }))
        .unwrap();
        runtime.configure(&configuration).await.unwrap();
        std::fs::create_dir(root.join("vessel")).unwrap();
        // The provider prepares linux/amd64 guests, so the stand-in vessel
        // reports that platform on any test host.
        let gateway = Gateway::start_as(&root.join("vessel"), reverse_forwards, BUILD, crate::guest_tools::linux_amd64()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        // A file for the tool-asset test to push.
        std::fs::write(bin.join("tar"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(bin.join("tar"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { _directory: directory, root, runtime, client, gateway, task }
    }
    async fn ready(&self) {
        self.runtime.credential("reference", wire::Secret::new("reference-only-test-key")).await.unwrap();
    }
    fn provider(&self) -> DrydockEnvironmentProvider {
        self.provider_through(self.gateway.clone())
    }
    fn provider_through(&self, dialer: Arc<dyn SshDialer>) -> DrydockEnvironmentProvider {
        DrydockEnvironmentProvider::new(Arc::new(self.client.clone()), "reference".into(), "reference".into(), self.root.join("keys"))
            .with_agent_reach(AgentReach::new(AgentLink::default()).with_dialer(dialer).with_timeout(Duration::from_secs(2)))
            .with_agent_build(BUILD.to_vec())
            .with_attach_root(self.root.join("attach"))
    }
    /// Provision inputs whose guest processes find the faked tools first.
    fn opts(&self) -> ProvisionOpts {
        ProvisionOpts { tokens: vec![("PATH".into(), format!("{}:/usr/bin:/bin", self.root.join("bin").display()))], ..Default::default() }
    }
}
pub(super) fn spec() -> flotilla_resources::EnvironmentSpec {
    flotilla_resources::EnvironmentSpec {
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
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
async fn fixture_prepared(_: &HostFixture, provider: &DrydockEnvironmentProvider) -> PreparedEnvironment {
    fixture_prepared_spec(provider, spec()).await
}
pub(super) async fn fixture_prepared_spec(
    provider: &DrydockEnvironmentProvider,
    input: flotilla_resources::EnvironmentSpec,
) -> PreparedEnvironment {
    if ::drydock::launch::build_tools_digest().is_ok() {
        return provider.prepare(&input, &Default::default()).await.unwrap();
    }
    assert!(provider.prepare(&input, &Default::default()).await.err().expect("unpublished tools refuse").contains("tools image"));
    let spec = input.cloud_sandbox.unwrap();
    let prepared = provider
        .api
        .prepare(
            "reference",
            &wire::PrepareRequest {
                platform: None,
                route: None,
                image_pull: None,
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
    assert!(host.provider().provision(EnvironmentId::new("crew"), &prepared, host.opts()).await.is_err());
    let first = provider.provision(EnvironmentId::new("crew"), &prepared, host.opts()).await.unwrap();
    let second = provider.provision(EnvironmentId::new("crew"), &prepared, host.opts()).await.unwrap();
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
    let private = key_dir(&provider.directory, first.id());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o700);
    provider.destroy(first.backing_id()).await.unwrap();
    assert!(!private.join("id_ed25519").exists());
    provider.destroy(first.backing_id()).await.unwrap();
    assert!(provider.list_backings().await.unwrap().is_empty());
}
// The Drydock checker invokes adapter lifecycle methods via an injected HTTP
// executor. Streams retain the real host; preparation follows the main reference fixture.
// Reach is drydock's own (its agent entry point is tested there), so the
// adapter is not checked for it.
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
        host: host.client.clone(),
        opts: host.opts(),
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

// One slow agent connection must not block another label; repeated adoption
// must refresh the existing guest's environment without reconnecting it.
#[tokio::test]
async fn independent_provisions_and_rotated_tokens() {
    /// Holds the first link until released; later ones pass straight through.
    struct Held {
        gateway: Arc<Gateway>,
        calls: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl SshDialer for Held {
        async fn dial(&self, config: drydock_transport::ssh::SshConfig) -> std::io::Result<drydock_transport::Transport> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.gateway.dial(config).await
        }
    }
    let host = HostFixture::start().await;
    host.ready().await;
    let held = Arc::new(Held {
        gateway: host.gateway.clone(),
        calls: Default::default(),
        entered: Default::default(),
        release: Default::default(),
    });
    let provider = host.provider_through(held.clone());
    let prepared = Arc::new(fixture_prepared(&host, &provider).await);
    let provider = Arc::new(provider);
    let task = {
        let (provider, prepared, opts) = (provider.clone(), prepared.clone(), host.opts());
        tokio::spawn(async move { provider.provision(EnvironmentId::new("slow"), &prepared, opts).await })
    };
    held.entered.notified().await;
    let fast = tokio::time::timeout(Duration::from_secs(3), provider.provision(EnvironmentId::new("fast"), &prepared, host.opts()))
        .await
        .unwrap()
        .unwrap();
    held.release.notify_one();
    let slow = task.await.unwrap().unwrap();
    // The stand-in serves both vessels from one agent; retire the slow one so
    // its configuration cannot race the rotation below.
    slow.destroy().await.unwrap();
    let mut opts = host.opts();
    opts.tokens.push(("TEST_TOKEN".into(), "rotated".into()));
    let adopted = provider.provision(EnvironmentId::new("fast"), &prepared, opts).await.unwrap();
    assert_eq!(adopted.backing_id(), fast.backing_id());
    assert_eq!(held.calls.load(std::sync::atomic::Ordering::SeqCst), 2, "adoption reuses the live agent link");
    assert_eq!(adopted.env_vars().await.unwrap().get("TEST_TOKEN").map(String::as_str), Some("rotated"));
    fast.destroy().await.unwrap();
}

// #3765 B1: after a controller restart, failed guest setup must leave the
// existing, work-carrying backing available for the next adoption attempt.
#[tokio::test]
async fn restart_failed_adoption_preserves_backing() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let first = provider.provision(EnvironmentId::new("survivor"), &prepared, host.opts()).await.unwrap();
    let backing = first.backing_id().to_owned();
    drop(first);
    drop(provider);
    host.gateway.stop();
    let restarted = host.provider();
    let prepared = fixture_prepared(&host, &restarted).await;
    let failure = restarted.provision(EnvironmentId::new("survivor"), &prepared, host.opts()).await.err().expect("no agent answers");
    assert!(
        matches!(&failure, EnvironmentProvisionError::RetryAdoption(diagnostic) if diagnostic.contains("did not complete its hello")),
        "{failure}"
    );
    assert_eq!(host.client.inspect("reference", &backing).await.unwrap().id, backing);
}

// #3765 S3: an agent that stops answering makes the vessel Unknown, never Gone.
// Once one answers again, liveness reconnects through drydock and restores the
// configured environment and the lab forward without recreating the provider;
// refreshing the lab name never duplicates its hosts entry.
#[tokio::test]
async fn lost_agent_reconnects_on_liveness() {
    let host = HostFixture::start().await;
    host.ready().await;
    let lab = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let guest_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let hosts = host.root.join("hosts");
    tokio::fs::write(&hosts, b"127.0.0.1 localhost\n").await.unwrap();
    let mut provider = host.provider().with_lab_reach(Some(("127.0.0.1".into(), lab.local_addr().unwrap().port())));
    provider.lab_guest = LabGuest {
        tcp: std::net::SocketAddr::from(([127, 0, 0, 1], guest_port)),
        socket: host.root.join("vessel/lab.sock"),
        hosts: hosts.clone(),
    };
    let prepared = fixture_prepared(&host, &provider).await;
    let mut opts = host.opts();
    opts.tokens.push(("TOKEN".into(), "rotated".into()));
    let handle = provider.provision(EnvironmentId::new("recovery"), &prepared, opts.clone()).await.unwrap();
    provider.provision(EnvironmentId::new("recovery"), &prepared, opts).await.unwrap();
    assert!(matches!(handle.observe().await.state, ObservationState::Running));
    host.gateway.stop();
    assert!(matches!(handle.observe().await.state, ObservationState::Unknown { .. }));
    assert!(matches!(host.client.inspect("reference", handle.backing_id()).await.unwrap().state, wire::Observation::Running));
    host.gateway.restart().unwrap();
    let observation = handle.observe().await;
    assert!(matches!(observation.state, ObservationState::Running), "{observation:?}");
    assert_eq!(handle.env_vars().await.unwrap().get("TOKEN").map(String::as_str), Some("rotated"));
    let mut guest = tokio::net::TcpStream::connect(("127.0.0.1", guest_port)).await.unwrap();
    guest.write_all(b"lab").await.unwrap();
    let (mut service, _) = tokio::time::timeout(Duration::from_secs(5), lab.accept()).await.unwrap().unwrap();
    let mut received = [0; 3];
    service.read_exact(&mut received).await.unwrap();
    assert_eq!(&received, b"lab");
    let names = tokio::fs::read_to_string(&hosts).await.unwrap();
    assert_eq!(names.matches(LAB_HOSTNAME).count(), 1, "{names}");
    handle.destroy().await.unwrap();
}

// #3765 F4 / review 4: incomplete adoption keys cannot be rotated by a
// label-idempotent POST. Preserve the existing pair and backing, report the
// missing key distinctly, and allow recovery after the original key is restored.
#[tokio::test]
async fn incomplete_adoption_keypair_requires_restore() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    for (index, missing) in ["id_ed25519", "id_ed25519.pub"].into_iter().enumerate() {
        let id = EnvironmentId::new(format!("repair-{index}"));
        let first = provider.provision(id.clone(), &prepared, host.opts()).await.unwrap();
        let directory = key_dir(&provider.directory, &id);
        let original = tokio::fs::read(directory.join(missing)).await.unwrap();
        tokio::fs::remove_file(directory.join(missing)).await.unwrap();
        let failure = provider.provision(id.clone(), &prepared, host.opts()).await.err().expect("adoption must not regenerate keys");
        assert!(matches!(failure, EnvironmentProvisionError::RetryAdoption(_)));
        assert!(failure.to_string().contains("SSH keypair incomplete or missing for adopted backing"));
        assert!(!directory.join(missing).exists());
        assert_eq!(host.client.inspect("reference", first.backing_id()).await.unwrap().id, first.backing_id());
        tokio::fs::write(directory.join(missing), original).await.unwrap();
        let recovered = provider.provision(id, &prepared, host.opts()).await.unwrap();
        assert_eq!(first.backing_id(), recovered.backing_id());
        recovered.destroy().await.unwrap();
    }
    // With no existing backing, a stale public half is safe to replace.
    let id = EnvironmentId::new("fresh-half-pair");
    let directory = key_dir(&provider.directory, &id);
    ::drydock::local::private_dir(&directory).unwrap();
    tokio::fs::write(directory.join("id_ed25519.pub"), "stale public half").await.unwrap();
    let fresh = provider.provision(id, &prepared, host.opts()).await.unwrap();
    assert!(directory.join("id_ed25519").is_file());
    assert_ne!(tokio::fs::read_to_string(directory.join("id_ed25519.pub")).await.unwrap(), "stale public half");
    fresh.destroy().await.unwrap();
}

// #3765 liveness: an inventory snapshot collected before creation must not
// override direct inspection of a live backing. Pin that interleaving explicitly.
#[tokio::test]
async fn stale_snapshot_absence_is_confirmed_by_inspect() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("snapshot-race"), &prepared, host.opts()).await.unwrap();
    let backing = host.client.inspect("reference", handle.backing_id()).await.unwrap();
    let adopted = provider.handle(backing, spec().cloud_sandbox.unwrap().image, BTreeMap::new(), false).unwrap();
    *adopted.watch.state.write().unwrap() = Some(ObservationState::Gone { cause: GoneCause::MissingOnList });
    assert!(matches!(adopted.observe().await.state, ObservationState::Running));
    handle.destroy().await.unwrap();
}

// Behaviour (#3980 contracts 2, 3 and 7; #3981): through either agent link
// drydock chooses, the vessel agent serves a controller socket at a guest path
// the guest dials, and pushes tool files. A forward the agent refuses fails
// naming the forward, without waiting. What drydock chose (#3980 contract 9)
// is recorded in the provider descriptor.
#[tokio::test]
async fn guest_sockets_are_agent_forwards_through_either_link() {
    for (reverse_forwards, link) in [(false, "local_forward"), (true, "reverse_forward")] {
        let host = HostFixture::with_gateway(reverse_forwards).await;
        host.ready().await;
        let provider = host.provider();
        let prepared = fixture_prepared(&host, &provider).await;
        let daemon = tokio::net::UnixListener::bind(host.root.join("daemon.sock")).unwrap();
        let guest_socket = host.root.join("vessel/run/daemon.sock");
        let handle = provider.provision(EnvironmentId::new("forwards"), &prepared, host.opts()).await.unwrap();
        let report = handle.provider_descriptor().unwrap().agent_connection.expect("drydock's report is recorded");
        let expected = flotilla_protocol::provider::AgentConnectionReport {
            provider: "reference".into(),
            module: "memory".into(),
            vessel: handle.backing_id().into(),
            endpoint: "gateway.test:22".into(),
            mode: link.into(),
        };
        assert_eq!(report, expected);
        let agent = handle.agent();
        agent.forward_controller_socket(&guest_socket, &host.root.join("daemon.sock")).await.unwrap();
        let tool = EnvironmentTool::new("flotilla", host.root.join("vessel/opt/flotilla")).with_asset(EnvironmentToolAsset::new(
            host.root.join("bin/tar"),
            host.root.join("vessel/opt/flotilla/tool"),
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "a tool",
        ));
        agent.push_tools(&[tool]).await.unwrap();
        let mut guest = tokio::net::UnixStream::connect(&guest_socket).await.unwrap();
        guest.write_all(b"crew").await.unwrap();
        let (mut controller, _) = tokio::time::timeout(Duration::from_secs(5), daemon.accept()).await.unwrap().unwrap();
        let mut received = [0; 4];
        controller.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"crew");
        use std::os::unix::fs::PermissionsExt;
        let pushed = std::fs::metadata(host.root.join("vessel/opt/flotilla/tool")).unwrap();
        assert_eq!(pushed.permissions().mode() & 0o777, 0o755);
        // A guest path whose parent is a file cannot be listened on.
        std::fs::write(host.root.join("hosts-file"), b"").unwrap();
        let (unservable, daemon_path) = (host.root.join("hosts-file/daemon.sock"), host.root.join("daemon.sock"));
        let refused = agent.forward_controller_socket(&unservable, &daemon_path);
        let message = tokio::time::timeout(Duration::from_secs(5), refused)
            .await
            .expect("a refused forward never hangs")
            .expect_err("a refused forward fails");
        assert!(message.contains("guest forward socket-") && message.contains("hosts-file/daemon.sock"), "{message}");
        handle.destroy().await.unwrap();
    }
}
