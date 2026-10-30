use super::*;
use ::drydock::{
    agent::{stand_in::Gateway, SshDialer},
    client::Client,
    conformance,
    daemon::{AgentBuilds, Daemon, HostOption, Seams},
};
use hegel::generators as gs;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

/// The agent build the stand-in vessel runs, and drydock and the provider push.
const BUILD: &[u8] = b"flotilla drydock test agent";

/// Drydock's agent build seam, given the provider's own build.
struct Build;
#[async_trait]
impl AgentBuilds for Build {
    async fn build(&self, _: &wire::Platform) -> Result<Vec<u8>, String> {
        Ok(BUILD.to_vec())
    }
}

/// Tag-free images: prepare already names digests.
struct Digests;
#[async_trait]
impl ::drydock::launch::ImageResolver for Digests {
    async fn resolve(&self, image: &str) -> Result<String, ::drydock::client::Error> {
        Ok(image.into())
    }
}

/// Drydock's daemon, as flotillad embeds it, over a real Rust host with the
/// reference module, whose vessels declare an agent reached over SSH, and an
/// in-process gateway serving a real agent. The stand-in vessel is this host:
/// its workspace root, agent socket, hosts file and guest sockets are the
/// fixture's.
struct HostFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    gateway: Arc<Gateway>,
    dialer: Arc<dyn SshDialer>,
    host: tokio::task::JoinHandle<()>,
    daemon: Option<(Arc<Daemon>, tokio::task::JoinHandle<()>)>,
    /// The daemon's API socket, as a socket-backed caller sees it.
    client: Client,
    /// Whether the host carries the agent connection itself (a relay reach),
    /// instead of declaring SSH.
    relayed: bool,
}
impl Drop for HostFixture {
    fn drop(&mut self) {
        self.host.abort();
        if let Some((_, server)) = self.daemon.take() {
            server.abort();
        }
    }
}
impl HostFixture {
    async fn start() -> Self {
        Self::start_through(|gateway| gateway).await
    }
    /// With the host relaying to the stand-in vessel's agent, as drydock's `local` module does.
    async fn start_relayed() -> Self {
        Self::start_with(|gateway| gateway, true).await
    }
    /// With drydock's SSH link to the vessel made by `dialer`, given the gateway.
    async fn start_through(dialer: impl FnOnce(Arc<Gateway>) -> Arc<dyn SshDialer>) -> Self {
        Self::start_with(dialer, false).await
    }
    async fn start_with(dialer: impl FnOnce(Arc<Gateway>) -> Arc<dyn SshDialer>, relayed: bool) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = directory.path().to_path_buf();
        std::fs::create_dir(root.join("vessel")).unwrap();
        std::fs::write(root.join("hosts"), "127.0.0.1 localhost\n").unwrap();
        // The provider prepares linux/amd64 guests, so the stand-in vessel
        // reports that platform on any test host.
        let gateway = Gateway::start_as(&root.join("vessel"), BUILD, crate::guest_tools::linux_amd64()).unwrap();
        let socket = root.join("host.sock");
        let serving = socket.clone();
        let host = tokio::spawn(async move {
            drydock_host::Host::new(drydock_host::reference::registry()).serve_unix(serving, std::future::pending()).await.unwrap();
        });
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        // A file for the tool-asset test to push.
        std::fs::write(bin.join("tar"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(bin.join("tar"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let dialer = dialer(gateway.clone());
        let mut fixture = Self {
            _directory: directory,
            root,
            gateway,
            dialer,
            host,
            daemon: None,
            client: Client::unix("/nonexistent").unwrap(),
            relayed,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while Client::unix(&socket).unwrap().hello().await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fixture.start_daemon().await;
        fixture
    }
    fn daemon(&self) -> Arc<Daemon> {
        self.daemon.as_ref().expect("drydock runs").0.clone()
    }
    async fn start_daemon(&mut self) {
        let reach = if self.relayed {
            format!("relay_socket='{}'", self.root.join("vessel/agent.sock").display())
        } else {
            "reach={kind='ssh', endpoint='gateway.test:22', user='minted-gateway-user', host_key='ssh-ed25519 AAAA'}".into()
        };
        let text = format!(
            "[providers.reference]\nmodule='reference'\ncredential={{file='{}'}}\n\
             settings={{fixture=true, agent=true, poll_interval_ms=10, workspace_root='{}', {reach}}}\n",
            self.root.join("credential").display(),
            self.root.join("workspace").display()
        );
        let run = self.root.join("run");
        let seams = Seams {
            dialer: self.dialer.clone(),
            builds: Arc::new(Build),
            guest: ::drydock::exposures::GuestLayout { hosts: self.root.join("hosts"), sockets: self.root.join("guest") },
            timing: ::drydock::exposures::Timing {
                connect: Duration::from_secs(3),
                retry: Duration::from_millis(100),
                heartbeat: Duration::from_millis(200),
            },
            ..Seams::new(Arc::new(Digests))
        };
        let option = HostOption {
            optional: false,
            name: "rust".into(),
            connection: ::drydock::HostConnection::ExternalUnix { path: self.root.join("host.sock") },
        };
        let config = ::drydock::config::Config::parse(&text).unwrap();
        let daemon = Daemon::start_with_seams(config, &run, run.join("keys"), vec![option], seams).await.unwrap();
        let (serving, socket) = (daemon.clone(), run.join("api.sock"));
        let path = socket.clone();
        let server = tokio::spawn(async move { serving.serve(&path, std::future::pending()).await.unwrap() });
        self.client = Client::unix(&socket).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.client.hello().await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        self.daemon = Some((daemon, server));
    }
    /// Drydock stops, closing what it held, and starts again from its files.
    /// Every provider and handle on the old daemon must be dropped first.
    async fn restart_daemon(&mut self) {
        let (daemon, server) = self.daemon.take().unwrap();
        server.abort();
        let _ = server.await;
        // Closing the client's connections ends the server's tasks for them.
        self.client = Client::unix("/nonexistent").unwrap();
        // Aborted tasks release their clones once the runtime drops them.
        tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::strong_count(&daemon) > 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("nothing else holds the stopped daemon");
        drop(daemon);
        self.start_daemon().await;
    }
    /// Give the provider its credential where drydock reads it, and wait for
    /// drydock to report it ready.
    async fn ready(&self) {
        let daemon = self.daemon();
        std::fs::write(self.root.join("credential"), "reference-only-test-key").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(ProviderApi::provider(&*daemon, "reference").await.map(|p| p.readiness), Ok(wire::Readiness::Ready)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("drydock reports the delivered credential");
    }
    fn provider(&self) -> DrydockEnvironmentProvider {
        let mut provider =
            DrydockEnvironmentProvider::new(self.daemon(), "reference".into(), "reference".into()).with_agent_build(BUILD.to_vec());
        provider.guest = GuestLayout { agent: self.root.join("vessel/agent.sock"), lab_port: 443 };
        provider.agent_timeout = Duration::from_secs(5);
        provider
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
/// Bytes written on `from` arrive at what `accept` yields, and its reply comes back.
async fn round_trip<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin, F: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut from: S,
    accept: impl std::future::Future<Output = F>,
) {
    from.write_all(b"ping").await.unwrap();
    let mut far = tokio::time::timeout(Duration::from_secs(5), accept).await.expect("the far end is reached");
    let mut ping = [0; 4];
    far.read_exact(&mut ping).await.unwrap();
    assert_eq!(&ping, b"ping");
    far.write_all(b"pong").await.unwrap();
    let mut pong = [0; 4];
    from.read_exact(&mut pong).await.unwrap();
    assert_eq!(&pong, b"pong");
}

/// Exposure names drydock announces within `window`.
async fn announced(events: &mut ::drydock::api::ObservationStream, window: Duration) -> Vec<String> {
    use futures::StreamExt;
    let mut names = Vec::new();
    let _ = tokio::time::timeout(window, async {
        while let Some(Ok(record)) = events.next().await {
            if let wire::ObserveRecord::Event { event } = record {
                if let wire::EventKind::Exposure { name, .. } = event.event {
                    names.push(name);
                }
            }
        }
    })
    .await;
    names
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
    let declared = provider.declaration.read().unwrap().clone().expect("readiness loaded the real declaration");
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
    provider.destroy(first.backing_id()).await.unwrap();
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

// #3620: a configured instance remains advertised as missing-tool when the
// host that would load its module cannot start. Drydock's daemon owns this.
#[tokio::test]
async fn missing_host_binary_is_not_ready() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let option = HostOption {
        name: "missing".into(),
        optional: true,
        connection: ::drydock::HostConnection::Managed { command: vec!["missing-language-host".into()], runtime: None },
    };
    let config = ::drydock::config::Config::parse("[providers.configured]\nmodule='daytona'\n").unwrap();
    let run = directory.path().join("run");
    let daemon = Daemon::start_with_seams(config, &run, run.join("keys"), vec![option], Seams::new(Arc::new(Digests))).await.unwrap();
    let provider = DrydockEnvironmentProvider::new(daemon, "configured".into(), "daytona".into());
    assert!(matches!(provider.detect_readiness().await, ProviderReadiness::MissingTool { .. }));
}

// One slow vessel connection must not block another label; repeated adoption
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
        async fn dial(&self, config: drydock_transport::ssh::SshConfig) -> std::io::Result<drydock_transport::ByteStream> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.gateway.dial(config).await
        }
    }
    let held = std::sync::OnceLock::new();
    let host = HostFixture::start_through(|gateway| {
        let dialer = Arc::new(Held { gateway, calls: Default::default(), entered: Default::default(), release: Default::default() });
        held.set(dialer.clone()).ok();
        dialer
    })
    .await;
    let held = held.get().unwrap().clone();
    host.ready().await;
    let provider = host.provider();
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
    assert_eq!(held.calls.load(std::sync::atomic::Ordering::SeqCst), 2, "adoption reuses the live vessel link");
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
        matches!(&failure, EnvironmentProvisionError::RetryAdoption(diagnostic) if diagnostic.contains("agent through drydock")),
        "{failure}"
    );
    assert_eq!(host.client.inspect("reference", &backing).await.unwrap().id, backing);
}

// #3765 S3: an agent that stops answering makes the vessel Unknown, never Gone.
// Once one answers again, drydock re-establishes the vessel's link and liveness
// reconnects through its exposure, restoring the configured environment,
// without recreating the provider.
#[tokio::test]
async fn lost_agent_reconnects_on_liveness() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let mut opts = host.opts();
    opts.tokens.push(("TOKEN".into(), "rotated".into()));
    let handle = provider.provision(EnvironmentId::new("recovery"), &prepared, opts).await.unwrap();
    assert!(matches!(handle.observe().await.state, ObservationState::Running));
    host.gateway.stop();
    assert!(matches!(handle.observe().await.state, ObservationState::Unknown { .. }));
    assert!(matches!(host.client.inspect("reference", handle.backing_id()).await.unwrap().state, wire::Observation::Running));
    host.gateway.restart().unwrap();
    let observation = handle.observe().await;
    assert!(matches!(observation.state, ObservationState::Running), "{observation:?}");
    assert_eq!(handle.env_vars().await.unwrap().get("TOKEN").map(String::as_str), Some("rotated"));
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
    let handle = provider.provision(EnvironmentId::new("snapshot-race"), &prepared, host.opts()).await.unwrap();
    let backing = host.client.inspect("reference", handle.backing_id()).await.unwrap();
    let adopted = provider.handle(backing, spec().cloud_sandbox.unwrap().image, BTreeMap::new(), false).unwrap();
    *adopted.watch.state.write().unwrap() = Some(ObservationState::Gone { cause: GoneCause::MissingOnList });
    assert!(matches!(adopted.observe().await.state, ObservationState::Running));
    handle.destroy().await.unwrap();
}

// drydock#132 case 4, drive the agent: the provider holds no vessel key and
// opens no link of its own. Its agent is reached through the inward exposure
// of the agent's socket, which it reports, and pushes tool files.
#[tokio::test]
async fn the_agent_is_driven_through_an_inward_exposure() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("driven"), &prepared, host.opts()).await.unwrap();
    let agent = host.daemon().inward("reference", handle.backing_id(), "agent").await.unwrap();
    assert_eq!(agent.target, wire::InwardTarget::Unix(host.root.join("vessel/agent.sock").display().to_string()));
    let wire::Local::Unix(local) = agent.local;
    let expected = AgentConnectionReport {
        provider: "reference".into(),
        module: "reference".into(),
        vessel: handle.backing_id().into(),
        endpoint: local,
        mode: "inward_exposure".into(),
    };
    assert_eq!(handle.provider_descriptor().unwrap().agent_connection, Some(expected));
    let tool = EnvironmentTool::new("flotilla", host.root.join("vessel/opt/flotilla")).with_asset(EnvironmentToolAsset::new(
        host.root.join("bin/tar"),
        host.root.join("vessel/opt/flotilla/tool"),
        EnvironmentToolAssetKind::File,
        EnvironmentToolAssetAccess::ReadOnly,
        "a tool",
    ));
    handle.agent().push_tools(&[tool]).await.unwrap();
    use std::os::unix::fs::PermissionsExt;
    let pushed = std::fs::metadata(host.root.join("vessel/opt/flotilla/tool")).unwrap();
    assert_eq!(pushed.permissions().mode() & 0o777, 0o755);
    handle.destroy().await.unwrap();
}

// A host that carries the connection to its vessel's agent itself (a relay
// reach, as drydock's `local` module's) has no agent socket at the guest path:
// the agent is reached through drydock's relay route, and no exposure is made.
#[tokio::test]
async fn a_relayed_agent_is_reached_through_the_relay() {
    let host = HostFixture::start_relayed().await;
    host.ready().await;
    let mut provider = host.provider();
    provider.guest.agent = host.root.join("no-guest-agent.sock");
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("relayed"), &prepared, host.opts()).await.unwrap();
    let expected = AgentConnectionReport {
        provider: "reference".into(),
        module: "reference".into(),
        vessel: handle.backing_id().into(),
        endpoint: "relay".into(),
        mode: "relay".into(),
    };
    assert_eq!(handle.provider_descriptor().unwrap().agent_connection, Some(expected));
    assert!(host.daemon().inward("reference", handle.backing_id(), "agent").await.is_err());
    handle.agent().create_dir(&host.root.join("vessel/made-through-the-relay"), 0o700).await.unwrap();
    assert!(host.root.join("vessel/made-through-the-relay").is_dir());
    handle.destroy().await.unwrap();
}

// drydock#132 case 1, attach to cleat: the socket the provider hands back is
// the local end of an inward exposure, which reaches cleat's socket in the vessel.
#[tokio::test]
async fn cleat_attach_is_an_inward_exposure() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("attach"), &prepared, host.opts()).await.unwrap();
    let guest = host.root.join("vessel/cleat.sock");
    let cleat = UnixListener::bind(&guest).unwrap();
    let local = handle.agent().forward_socket(&guest).await.unwrap();
    let exposed = host.daemon().inwards("reference", handle.backing_id()).await.unwrap();
    assert!(exposed.iter().any(|inward| inward.local == wire::Local::Unix(local.display().to_string())), "{exposed:?}");
    round_trip(UnixStream::connect(&local).await.unwrap(), async { cleat.accept().await.unwrap().0 }).await;
    assert_eq!(handle.agent().forward_socket(&guest).await.unwrap(), local, "a standing exposure is reused");
    handle.destroy().await.unwrap();
}

// drydock#132 case 2, crew talks back: the guest socket is an outward exposure
// reaching the controller's socket. One drydock cannot set up fails naming the
// guest path, without waiting; a guest TCP listener is refused.
#[tokio::test]
async fn crew_talks_back_through_an_outward_exposure() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("crew"), &prepared, host.opts()).await.unwrap();
    let (controller, guest) = (host.root.join("daemon.sock"), host.root.join("vessel/run/daemon.sock"));
    let daemon = UnixListener::bind(&controller).unwrap();
    let agent = handle.agent();
    agent.create_dir(&host.root.join("vessel/run"), 0o700).await.unwrap();
    agent.forward_controller_socket(&guest, None, &controller).await.unwrap();
    round_trip(UnixStream::connect(&guest).await.unwrap(), async { daemon.accept().await.unwrap().0 }).await;
    // A guest path whose parent is a file cannot be listened on.
    std::fs::write(host.root.join("hosts-file"), b"").unwrap();
    let unservable = host.root.join("hosts-file/daemon.sock");
    let refused = agent.forward_controller_socket(&unservable, None, &controller);
    let message = tokio::time::timeout(Duration::from_secs(10), refused)
        .await
        .expect("a refused exposure never hangs")
        .expect_err("a refused exposure fails");
    assert!(message.contains("hosts-file/daemon.sock"), "{message}");
    let tcp = std::net::SocketAddr::from(([127, 0, 0, 1], 1));
    assert!(agent.forward_controller_socket(&guest, Some(tcp), &controller).await.is_err());
    handle.destroy().await.unwrap();
}

// drydock#132 case 3, lab service: the lab's name in the guest is an outward
// exposure drydock gives an address and a hosts line, reaching the lab from
// drydock's machine. Re-adoption leaves the standing exposure as it is.
#[tokio::test]
async fn the_lab_service_is_a_named_outward_exposure() {
    let host = HostFixture::start().await;
    host.ready().await;
    let lab = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let guest_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut provider = host.provider().with_lab_reach(Some(("127.0.0.1".into(), lab.local_addr().unwrap().port())));
    provider.guest.lab_port = guest_port;
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("lab"), &prepared, host.opts()).await.unwrap();
    let names = tokio::fs::read_to_string(host.root.join("hosts")).await.unwrap();
    let line = names.lines().find(|line| line.contains(LAB_HOSTNAME)).unwrap_or_else(|| panic!("{names}"));
    let address: std::net::IpAddr = line.split_whitespace().next().unwrap().parse().unwrap();
    round_trip(tokio::net::TcpStream::connect((address, guest_port)).await.unwrap(), async { lab.accept().await.unwrap().0 }).await;
    let mut events = ProviderApi::observe(&*host.daemon(), None).await.unwrap();
    provider.provision(EnvironmentId::new("lab"), &prepared, host.opts()).await.unwrap();
    assert!(!announced(&mut events, Duration::from_millis(500)).await.contains(&"lab".to_owned()), "re-adoption sets no lab exposure");
    let names = tokio::fs::read_to_string(host.root.join("hosts")).await.unwrap();
    assert_eq!(names.matches(LAB_HOSTNAME).count(), 1, "{names}");
    handle.destroy().await.unwrap();
}

// Drydock keeps a vessel's exposures across its own restart, as flotillad's
// restart restarts it: the crew still talks back with nothing set up again,
// and a restarted provider adopts the vessel and drives its agent.
#[tokio::test]
async fn exposures_outlast_a_drydock_restart() {
    let mut host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let handle = provider.provision(EnvironmentId::new("restart"), &prepared, host.opts()).await.unwrap();
    let (controller, guest) = (host.root.join("daemon.sock"), host.root.join("vessel/crew.sock"));
    let daemon = UnixListener::bind(&controller).unwrap();
    handle.agent().forward_controller_socket(&guest, None, &controller).await.unwrap();
    let backing = handle.backing_id().to_owned();
    drop((handle, provider, prepared));
    host.restart_daemon().await;
    // Drydock takes its exposures up again in its own time; nothing here sets one.
    let drydock = host.daemon();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !drydock.outwards("reference", &backing).await.is_ok_and(|outwards| outwards.iter().all(|outward| outward.ready)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("drydock re-establishes the crew socket");
    round_trip(UnixStream::connect(&guest).await.unwrap(), async { daemon.accept().await.unwrap().0 }).await;
    let provider = host.provider();
    let prepared = fixture_prepared(&host, &provider).await;
    let adopted = provider.provision(EnvironmentId::new("restart"), &prepared, host.opts()).await.unwrap();
    assert!(!adopted.created_for_setup());
    assert!(adopted.env_vars().await.unwrap().contains_key("PATH"));
    adopted.destroy().await.unwrap();
}

/// A provider's refusal reaches the operator: the Environment's message
/// carries drydock's reason and the provider's (masked) explanation.
#[test]
fn a_protocol_error_keeps_the_providers_explanation() {
    let error = ::drydock::client::Error::Protocol(wire::ProtocolError::Internal {
        reason: "provider daytona operation provision".into(),
        provider_error: "Total memory limit exceeded".into(),
    });
    let message = api_error(error);
    assert!(message.contains("provision") && message.contains("Total memory limit exceeded"), "{message}");
}
