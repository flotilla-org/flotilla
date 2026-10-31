//! The adapter against an in-memory drydock and vessel agent. Drydock's own
//! daemon, hosts and agent are exercised where the composition root's
//! implementation of the API lives.
use super::{memory::MemoryApi, *};
use crate::environment::agent::memory::{MemoryAgent, MemoryLink, TOOLS_IMAGE};
use hegel::generators as gs;

struct Fixture {
    api: Arc<MemoryApi>,
    agent: Arc<MemoryAgent>,
    _directory: tempfile::TempDir,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let (api, _) = MemoryApi::new("memory", directory.path().join("agent.sock"));
        api.declaration.lock().unwrap().capabilities.workspace_root = workspace.display().to_string();
        Self { api, agent: MemoryAgent::new(), _directory: directory, workspace }
    }
    fn provider(&self) -> DrydockEnvironmentProvider {
        DrydockEnvironmentProvider::new(self.api.clone(), Arc::new(MemoryLink(self.agent.clone())), "memory".into(), "memory".into())
            .with_agent_build(b"agent build".to_vec())
            .with_agent_timeout(Duration::from_secs(1))
    }
}

fn spec() -> flotilla_resources::EnvironmentSpec {
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

// The declared capabilities are reported only as far as this adapter
// implements them; an instance drydock does not serve is not ready.
#[tokio::test]
async fn readiness_and_capabilities_follow_the_declaration() {
    let fixture = Fixture::new();
    let provider = fixture.provider();
    assert!(provider.capabilities().material_modes.is_empty(), "unknown declarations cannot claim Shared");
    fixture.api.declaration.lock().unwrap().readiness = wire::Readiness::MissingCredential { reason: "no key".into() };
    assert!(matches!(provider.detect_readiness().await, ProviderReadiness::MissingCredential { .. }));
    assert!(provider.prepare(&spec(), &Default::default()).await.is_err());
    fixture.api.declaration.lock().unwrap().readiness = wire::Readiness::Ready;
    assert_eq!(provider.detect_readiness().await, ProviderReadiness::Ready);
    let capabilities = provider.capabilities();
    assert_eq!(capabilities.material_modes, vec![MaterialMode::Frozen, MaterialMode::Pushed]);
    assert_eq!(capabilities.checkout_strategies, vec![CheckoutStrategy::CloneInVessel]);
    assert_eq!(capabilities.egress_enforcement, EgressEnforcement::Advisory);
    let absent =
        DrydockEnvironmentProvider::new(fixture.api.clone(), Arc::new(MemoryLink(fixture.agent.clone())), "other".into(), "other".into());
    assert!(matches!(absent.detect_readiness().await, ProviderReadiness::Unknown { reason } if reason.contains("absent")));
}

// Preparation names the link's tools image; provision is idempotent by label,
// recovery keeps opaque backing IDs, and repeated destroy is safe.
#[tokio::test]
async fn lifecycle_through_environment_provider() {
    let fixture = Fixture::new();
    let provider = fixture.provider();
    let prepared = provider.prepare(&spec(), &Default::default()).await.unwrap();
    assert_eq!(prepared.get::<Prepared>(&provider.owner).unwrap().prepared.tools_digest, TOOLS_IMAGE);
    assert!(
        fixture.provider().provision(EnvironmentId::new("crew"), &prepared, Default::default()).await.is_err(),
        "another instance's preparation"
    );
    let first = provider.provision(EnvironmentId::new("crew"), &prepared, Default::default()).await.unwrap();
    assert!(first.created_for_setup());
    let second = provider.provision(EnvironmentId::new("crew"), &prepared, Default::default()).await.unwrap();
    assert_eq!(first.backing_id(), second.backing_id());
    assert!(fixture.workspace.is_dir(), "set-up makes the workspace root");
    let descriptor = first.provider_descriptor().unwrap();
    assert_eq!((descriptor.host_version.as_str(), descriptor.module_version.as_str()), ("memory-1", "module-1"));
    assert_eq!(descriptor.image_digest, spec().cloud_sandbox.unwrap().image);
    assert_eq!(first.local_image_id(), Some(format!("sha256:{}", "a".repeat(64)).as_str()));
    assert_eq!(
        provider.list_backings().await.unwrap(),
        vec![EnvironmentBacking { environment_id: EnvironmentId::new("crew"), backing_id: first.backing_id().into() }]
    );
    assert_eq!(first.observe().await.state, ObservationState::Running);
    provider.destroy(first.backing_id()).await.unwrap();
    provider.destroy(first.backing_id()).await.unwrap();
    assert!(provider.list_backings().await.unwrap().is_empty());
}

// The agent is reached through the inward exposure of its socket, which the
// handle reports; tokens reach the agent's configuration, and re-adoption
// applies rotated ones to the same vessel.
#[tokio::test]
async fn the_agent_is_driven_through_an_inward_exposure() {
    let fixture = Fixture::new();
    let provider = fixture.provider();
    let prepared = provider.prepare(&spec(), &Default::default()).await.unwrap();
    let opts = |token: &str| ProvisionOpts { tokens: vec![("TOKEN".into(), token.into())], ..Default::default() };
    let handle = provider.provision(EnvironmentId::new("driven"), &prepared, opts("first")).await.unwrap();
    let agent = fixture.api.inward("memory", handle.backing_id(), "agent").await.unwrap();
    assert_eq!(agent.target, wire::InwardTarget::Unix("/run/drydock/agent.sock".into()));
    let wire::Local::Unix(local) = agent.local;
    let expected = AgentConnectionReport {
        provider: "memory".into(),
        module: "memory".into(),
        vessel: handle.backing_id().into(),
        endpoint: local,
        mode: "inward_exposure".into(),
    };
    assert_eq!(handle.provider_descriptor().unwrap().agent_connection, Some(expected));
    assert_eq!(handle.env_vars().await.unwrap().get("TOKEN").map(String::as_str), Some("first"));
    let adopted = provider.provision(EnvironmentId::new("driven"), &prepared, opts("rotated")).await.unwrap();
    assert_eq!(adopted.backing_id(), handle.backing_id());
    assert_eq!(adopted.env_vars().await.unwrap().get("TOKEN").map(String::as_str), Some("rotated"));
}

// An agent that stops answering makes a running vessel Unknown, never Gone,
// until it answers again.
#[tokio::test]
async fn an_unanswering_agent_is_unknown() {
    let fixture = Fixture::new();
    let provider = fixture.provider();
    let prepared = provider.prepare(&spec(), &Default::default()).await.unwrap();
    let handle = provider.provision(EnvironmentId::new("recovery"), &prepared, Default::default()).await.unwrap();
    fixture.agent.stop();
    assert!(matches!(handle.observe().await.state, ObservationState::Unknown { .. }));
    fixture.agent.restart();
    assert_eq!(handle.observe().await.state, ObservationState::Running);
}

// A refusal to adopt a backing this label already names keeps that backing
// for the next attempt; any other refusal fails the provision.
#[tokio::test]
async fn a_refused_adoption_is_retried_and_keeps_the_backing() {
    let fixture = Fixture::new();
    let provider = fixture.provider();
    let prepared = provider.prepare(&spec(), &Default::default()).await.unwrap();
    *fixture.api.refuse_provision.lock().unwrap() = Some("no key for this backing".into());
    let fresh = provider.provision(EnvironmentId::new("survivor"), &prepared, Default::default()).await.err().expect("refused");
    assert!(matches!(&fresh, EnvironmentProvisionError::Failed(message) if message.contains("no key for this backing")), "{fresh}");
    *fixture.api.refuse_provision.lock().unwrap() = None;
    let first = provider.provision(EnvironmentId::new("survivor"), &prepared, Default::default()).await.unwrap();
    *fixture.api.refuse_provision.lock().unwrap() = Some("no key for this backing".into());
    let failure = fixture.provider().provision(EnvironmentId::new("survivor"), &prepared, Default::default()).await;
    assert!(failure.is_err(), "another instance's preparation");
    let restarted = fixture.provider();
    let prepared = restarted.prepare(&spec(), &Default::default()).await.unwrap();
    let failure = restarted.provision(EnvironmentId::new("survivor"), &prepared, Default::default()).await.err().expect("refused");
    assert!(
        matches!(&failure, EnvironmentProvisionError::RetryAdoption(reason) if reason.contains("no key for this backing")),
        "{failure}"
    );
    assert!(fixture.api.inspect("memory", first.backing_id()).await.is_ok());
}

// An inventory snapshot collected before creation must not override direct
// inspection of a live backing.
#[tokio::test]
async fn stale_snapshot_absence_is_confirmed_by_inspect() {
    let fixture = Fixture::new();
    let provider = fixture.provider();
    let prepared = provider.prepare(&spec(), &Default::default()).await.unwrap();
    let handle = provider.provision(EnvironmentId::new("snapshot-race"), &prepared, Default::default()).await.unwrap();
    let backing = fixture.api.inspect("memory", handle.backing_id()).await.unwrap();
    let adopted = provider.handle(backing, spec().cloud_sandbox.unwrap().image, BTreeMap::new(), false).unwrap();
    *adopted.watch.state.write().unwrap() = Some(ObservationState::Gone { cause: GoneCause::MissingOnList });
    assert!(matches!(adopted.observe().await.state, ObservationState::Running));
    fixture.api.environments.lock().unwrap().clear();
    assert_eq!(adopted.observe().await.state, ObservationState::Gone { cause: GoneCause::MissingOnList });
}

// The lab service is a named outward exposure at its own name in the guest;
// re-adoption leaves a standing one as it is.
#[tokio::test]
async fn the_lab_service_is_a_named_outward_exposure() {
    let fixture = Fixture::new();
    let provider = fixture.provider().with_lab_reach(Some(("lab.internal".into(), 3000))).with_guest_lab_port(8443);
    let prepared = provider.prepare(&spec(), &Default::default()).await.unwrap();
    let handle = provider.provision(EnvironmentId::new("lab"), &prepared, Default::default()).await.unwrap();
    let lab = fixture.api.outward("memory", handle.backing_id(), "lab").await.unwrap();
    assert_eq!(lab.guest, wire::Guest::Name { name: LAB_HOSTNAME.into(), port: 8443 });
    assert_eq!(lab.destination, wire::OutwardDestination::Tcp("lab.internal:3000".into()));
    // A marker no put would keep: re-adoption leaves the standing exposure alone.
    let marker = wire::ExposureError { code: wire::ExposureErrorCode::Refused, reason: "standing".into() };
    let key = (handle.backing_id().to_owned(), "lab".to_owned());
    fixture.api.outwards.lock().unwrap().get_mut(&key).unwrap().error = Some(marker.clone());
    provider.provision(EnvironmentId::new("lab"), &prepared, Default::default()).await.unwrap();
    assert_eq!(fixture.api.outwards.lock().unwrap()[&key].error, Some(marker));
}

// Every terminal provider cause maps to Gone; a lifetime cap must never look
// resumable. Generate all causes and arbitrary process exit codes.
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
    assert_eq!(
        observation(wire::Observation::Unknown { reason: wire::UnknownReason::HostUnreachable }),
        ObservationState::Unknown { reason: UnknownReason::HostUnreachable }
    );
}

/// A provider's refusal reaches the operator with drydock's own description.
#[test]
fn a_protocol_error_keeps_the_providers_explanation() {
    let error = ApiError::Protocol {
        kind: wire::ProtocolErrorKind::Other,
        reason: "provider daytona operation provision".into(),
        description: "internal: provider daytona operation provision; Total memory limit exceeded".into(),
    };
    assert_eq!(api_error(error), "drydock provider internal: provider daytona operation provision; Total memory limit exceeded");
    assert_eq!(api_error(ApiError::NotReady), "drydock provider not ready");
}
