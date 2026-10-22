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
fn spec() -> flotilla_resources::EnvironmentSpec {
    flotilla_resources::EnvironmentSpec {
        cloud_sandbox: Some(flotilla_resources::CloudSandboxEnvironmentSpec {
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
    assert!(matches!(provider.detect_readiness().await, ProviderReadiness::MissingCredential { .. }));
    assert!(provider.prepare(&spec(), &Default::default()).await.is_err());
    host.ready().await;
    assert_eq!(provider.detect_readiness().await, ProviderReadiness::Ready);
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
    provider.destroy(first.backing_id()).await.unwrap();
    provider.destroy(first.backing_id()).await.unwrap();
    assert!(provider.list_backings().await.unwrap().is_empty());
}
// The Drydock checker receives the same real client used by this adapter and
// the adapter's prepared value (including Drydock's build-selected tools pin).
#[tokio::test]
async fn reference_passes_drydock_conformance() {
    let host = HostFixture::start().await;
    host.ready().await;
    let provider = host.provider();
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
    let report = conformance::run(&host.client, "reference", &options).await;
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
