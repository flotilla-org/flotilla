use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use super::*;
use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, EnvironmentSpec, ImageSource};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

// Docker's local image names as the CLI boundary sees them: `docker tag`
// records a name, and a derived build resolves each FROM line as BuildKit does.
#[derive(Default)]
pub(super) struct LocalImageTags(Mutex<BTreeMap<String, String>>);
impl LocalImageTags {
    pub(super) fn tag(&self, id: &str, tag: &str) -> Result<String, String> {
        assert!(flotilla_resources::is_image_digest(id), "only local image IDs are tagged: {id}");
        self.0.lock().unwrap().insert(tag.into(), id.into());
        Ok(String::new())
    }

    // BuildKit reads `FROM sha256:<hex>` as Docker Hub's `sha256` repository,
    // and an unknown tag also falls through to a registry pull. Registry
    // digest references resolve as written. Returns the local image ID the
    // final stage starts from, when it names a tagged local image.
    pub(super) fn resolve_build(&self, dockerfile: &str) -> Result<Option<String>, String> {
        let mut base = None;
        for image in
            dockerfile.lines().filter_map(|line| line.strip_prefix("FROM ")?.split_whitespace().find(|part| !part.starts_with("--")))
        {
            if image.contains('@') {
                base = None;
                continue;
            }
            if let Some(id) = self.0.lock().unwrap().get(image).filter(|_| !image.starts_with("sha256:")) {
                base = Some(id.clone());
                continue;
            }
            let name = if image.contains('/') { image.to_string() } else { format!("docker.io/library/{image}") };
            return Err(format!(
                "failed to resolve source metadata for {name}: pull access denied, repository does not exist or may require authorization"
            ));
        }
        Ok(base)
    }
}

// Process-boundary fake: a small Docker engine with persistent inventory. It
// refuses unexpected commands rather than returning success for arbitrary input.
#[derive(Default)]
struct DockerEngine {
    inventory: Mutex<BTreeMap<String, String>>,
    unavailable: AtomicBool,
    tags: LocalImageTags,
}
#[async_trait]
impl CommandRunner for DockerEngine {
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        if cmd == "cat" {
            return Err("no cgroup sample".into());
        }
        if cmd == "systemctl" {
            return Ok(super::docker_vessels::TEST_SLICE_SHOW.into());
        }
        assert_eq!(cmd, "docker");
        if self.unavailable.load(Ordering::Acquire) {
            return Err("provider API unavailable".into());
        }
        let mut inventory = self.inventory.lock().unwrap();
        match args {
            ["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", _] => Ok("linux/amd64".into()),
            ["image", "inspect", ..] => Ok("[]".into()),
            ["tag", id, tag] => self.tags.tag(id, tag),
            ["exec", "--user", "0", ..]
            | ["exec", _, "/opt/drydock/drydock-agent", "--supervise", "--socket", "/run/drydock/agent.sock"] => Ok(String::new()),
            ["inspect", "--format", "{{json .Config.Env}}", _] => Ok(r#"["HOME=/tmp","PATH=/usr/bin:/bin"]"#.into()),
            ["info", "--format", "{{json .}}"] => Ok(super::docker_vessels::TEST_DOCKER_INFO.into()),
            ["run", ..] => {
                let name = args.windows(2).find(|pair| pair[0] == "--name").unwrap()[1];
                let id = args.iter().find_map(|arg| arg.strip_prefix("FLOTILLA_ENVIRONMENT_ID=")).unwrap();
                assert!(!inventory.contains_key(name), "duplicate backing creation");
                inventory.insert(name.into(), id.into());
                Ok(immutable_id(id))
            }
            ["inspect", "--format", "{{.Image}}", name] => {
                assert!(inventory.contains_key(*name));
                Ok(DIGEST.into())
            }
            ["inspect", name] => {
                let id =
                    inventory.get(*name).or_else(|| inventory.values().find(|id| immutable_id(id) == *name)).ok_or("missing container")?;
                let mut record: serde_json::Value = serde_json::from_str(include_str!("fixtures/normal-stop.json")).unwrap();
                record[0]["Id"] = immutable_id(id).into();
                record[0]["State"]["Status"] = "running".into();
                Ok(record.to_string())
            }
            ["ps", ..] if args.last().unwrap().contains("{{.Image}}") => {
                Ok(inventory.iter().map(|(name, id)| format!("{} {name}\t{id}\t{DIGEST}\t[]\n", immutable_id(id))).collect())
            }
            ["ps", ..] => Ok(inventory.values().map(|id| format!("{}\t{id}\n", immutable_id(id))).collect()),
            ["rm", "-f", backing] => {
                assert!(backing.len() == 64 && backing.bytes().all(|byte| byte.is_ascii_hexdigit()), "rm must use immutable ID");
                inventory.retain(|_, id| immutable_id(id) != *backing);
                Ok(String::new())
            }
            _ => panic!("unexpected Docker invocation: {args:?}"),
        }
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        assert_eq!(cmd, "docker");
        assert_eq!(args, ["build", "--platform", "linux/amd64", "--quiet", "--network=none", "-"]);
        let dockerfile = std::str::from_utf8(input).unwrap();
        assert!(dockerfile.contains("COPY --from=tools /opt/drydock /opt/drydock"));
        self.tags.resolve_build(dockerfile)?;
        Ok(DIGEST.into())
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        Ok(CommandOutput { stdout: self.run(cmd, args, cwd, label).await?, stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        false
    }
}
#[cfg(unix)]
pub(super) fn docker_provider(runner: Arc<dyn CommandRunner>) -> docker::DockerEnvironmentProvider {
    use super::agent::{tests::MemoryAgent, AgentConnector};
    let directory = Arc::new(tempfile::tempdir().unwrap());
    let keep = directory.clone();
    let agents = Mutex::new(BTreeMap::<std::path::PathBuf, Arc<MemoryAgent>>::new());
    docker::DockerEnvironmentProvider::new(runner)
        .with_vessel_slice(Some(super::docker_vessels::DockerVessels::for_tests()))
        .with_agent_settings(docker::DockerAgentSettings {
            runtime_root: directory.path().into(),
            executable: directory.path().join("current-agent"),
        })
        .with_agent_connector(Arc::new(move |socket| {
            let _keep = &keep;
            agents.lock().unwrap().entry(socket.into()).or_insert_with(|| MemoryAgent::new(false)).clone() as Arc<dyn AgentConnector>
        }))
}

fn immutable_id(id: &str) -> String {
    format!("{:0>64}", id.strip_prefix("env-").unwrap())
}

fn providers() -> Vec<(Arc<dyn EnvironmentProvider>, flotilla_resources::EnvironmentSpec)> {
    let image_spec =
        legacy_environment_spec(&EnvironmentSpec { image: ImageSource::Registry(DIGEST.into()), token_env_vars: vec![] }).unwrap();
    vec![
        #[cfg(unix)]
        (Arc::new(docker_provider(Arc::new(DockerEngine::default()))), image_spec.clone()),
        (Arc::new(dummy::DummyEnvironmentProvider::new(Default::default())), image_spec),
        (
            Arc::new(host_direct::HostDirectEnvironmentProvider::new(Arc::new(DockerEngine::default()), Default::default())),
            flotilla_resources::EnvironmentSpec {
                cloud_sandbox: None,
                provider_address: None,
                host_direct: Some(flotilla_resources::HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/".into() }),
                docker: None,
            },
        ),
    ]
}

// ADR 0058: all instances share prepare/provision/inspect/list/destroy/observe.
// Generate interleaved lifecycles of two environments, including empty inventory,
// re-creation after destruction and duplicate reads. Process failures and exit
// classification have separate properties; this scenario uses successful tools.
#[hegel::test]
fn provider_lifecycle_contract(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(10));
    let operations: Vec<_> =
        (0..count).map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(1)), tc.draw(gs::booleans()))).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        for (provider, spec) in providers() {
            let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
            let mut live: BTreeMap<String, EnvironmentHandle> = BTreeMap::new();
            for (index, destroy) in &operations {
                let id = EnvironmentId::new(format!("env-{index}"));
                if *destroy {
                    if let Some(handle) = live.remove(id.as_str()) {
                        let backing =
                            provider.list_backings().await.unwrap().into_iter().find(|backing| backing.environment_id == id).unwrap();
                        if *index == 0 {
                            handle.destroy().await.unwrap();
                        } else {
                            provider.destroy(&backing.backing_id).await.unwrap();
                        }
                    }
                } else if !live.contains_key(id.as_str()) {
                    let handle = provider.provision(id.clone(), &prepared, Default::default()).await.unwrap();
                    assert_eq!(handle.id(), &id);
                    assert_eq!(handle.capabilities(), provider.capabilities());
                    assert_eq!(handle.observe().await.state, ObservationState::Running);
                    live.insert(id.to_string(), handle);
                }
                let listed = provider.list().await.unwrap();
                assert_eq!(listed.len(), live.len());
                for index in 0..2 {
                    let id = EnvironmentId::new(format!("env-{index}"));
                    let expected = live.contains_key(id.as_str());
                    assert_eq!(provider.inspect(&id).await.unwrap().is_some(), expected);
                    if let Some(handle) = provider.inspect(&id).await.unwrap() {
                        assert_eq!(handle.observe().await.state, ObservationState::Running);
                        let backing =
                            provider.list_backings().await.unwrap().into_iter().find(|backing| backing.environment_id == id).unwrap();
                        assert_eq!(handle.backing_id(), backing.backing_id);
                        assert_eq!(live[id.as_str()].backing_id(), backing.backing_id);
                    }
                }
            }
        }
    });
}

// Prepared values cannot cross instances, including same-implementation instances.
// The available cases exhaust the installed adapters; the public capability data
// agrees with optional accessors and the dummy has its own descriptive class.
#[tokio::test]
async fn provider_instances_and_declarations() {
    for ((provider, spec), (other, _)) in providers().into_iter().zip(providers()) {
        let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
        assert!(other.provision(EnvironmentId::new("wrong-owner"), &prepared, Default::default()).await.is_err());
        let declaration = provider.capabilities();
        assert!(!declaration.workspace_root.as_os_str().is_empty());
    }
    assert!(providers().iter().any(|(provider, _)| provider.kind() == ProviderKind::new(EnvironmentClass::Dummy, "dummy")));
}

// A provider error means Unknown, never Gone. The same handle recovers
// when the injected process boundary becomes available again.
#[tokio::test]
#[cfg(unix)]
async fn observation_recovers_after_provider_error() {
    let engine = Arc::new(DockerEngine::default());
    let provider: Arc<dyn EnvironmentProvider> = Arc::new(docker_provider(engine.clone()));
    let spec = legacy_environment_spec(&EnvironmentSpec { image: ImageSource::Registry(DIGEST.into()), token_env_vars: vec![] }).unwrap();
    let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
    let id = EnvironmentId::new("env-1");
    let handle = provider.provision(id.clone(), &prepared, Default::default()).await.unwrap();
    engine.unavailable.store(true, Ordering::Release);
    let unknown = handle.observe().await;
    assert!(matches!(unknown.state, ObservationState::Unknown { reason: UnknownReason::ProviderApiError { .. } }));
    engine.unavailable.store(false, Ordering::Release);
    let recovered = handle.observe().await;
    assert_eq!(recovered.state, ObservationState::Running);
    assert_eq!(handle.local_image_id(), Some(DIGEST));
    provider.destroy(handle.backing_id()).await.unwrap();
    // Authoritative absence comes from inventory; an unavailable inspect alone
    // cannot distinguish deletion from an API outage and remains Unknown.
    assert!(provider.inspect(&id).await.unwrap().is_none());
    assert!(matches!(handle.observe().await.state, ObservationState::Unknown { reason: UnknownReason::ProviderApiError { .. } }));
}

// Advertisements are additive data: missing fields decode to their declared
// defaults. Generate every top-level and nested capability field omission.
#[hegel::test]
fn advertisement_fields_default_when_omitted(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let mut available = providers();
    let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(available.len() - 1));
    let (provider, _) = available.remove(index);
    let advertisement = ProviderAdvertisement {
        instance: "configured".into(),
        kind: Some(provider.kind()),
        capabilities: provider.capabilities(),
        readiness: ProviderReadiness::Ready,
    };
    let mut value = serde_json::to_value(&advertisement).unwrap();
    let nested = tc.draw(gs::booleans());
    let object = if nested { value["capabilities"].as_object_mut().unwrap() } else { value.as_object_mut().unwrap() };
    let keys: Vec<_> = object.keys().cloned().collect();
    let omitted = &keys[tc.draw(gs::integers::<usize>().min_value(0).max_value(keys.len() - 1))];
    object.remove(omitted);
    let decoded: ProviderAdvertisement = serde_json::from_value(value).unwrap();
    let decoded = serde_json::to_value(decoded).unwrap();
    let default = serde_json::to_value(ProviderAdvertisement::default()).unwrap();
    let default_caps = serde_json::to_value(ProviderCapabilities::default()).unwrap();
    if nested {
        assert_eq!(decoded["capabilities"][omitted], default_caps[omitted]);
    } else {
        assert_eq!(decoded[omitted], default[omitted]);
    }
}

// Host/dummy destruction retires adoption without stopping the host. Retained
// handles report requested destruction, and inventories no longer list them.
#[tokio::test]
async fn host_and_dummy_retirement_is_observable_and_excluded_from_inventory() {
    for (provider, spec) in providers().into_iter().filter(|(provider, _)| provider.kind().implementation != "docker") {
        let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
        let id = EnvironmentId::new("env-0");
        let handle = provider.provision(id.clone(), &prepared, Default::default()).await.unwrap();
        handle.destroy().await.unwrap();
        assert_eq!(handle.observe().await.state, ObservationState::Gone { cause: GoneCause::DestroyedByRequest });
        assert!(provider.inspect(&id).await.unwrap().is_none());
        assert!(provider.list().await.unwrap().is_empty());
        assert!(provider.list_backings().await.unwrap().is_empty());
    }
}

// Missing descriptive identity never impersonates a dummy provider. One example
// exhausts this decoder default; omission of other fields is generated above.
#[test]
fn omitted_advertisement_kind_stays_absent() {
    let decoded: ProviderAdvertisement = serde_json::from_str(r#"{"instance":"unadvertised"}"#).unwrap();
    assert_eq!(decoded.kind, None);
}

// Readiness probes the provider endpoint, not the daemon's build platform.
// Generated boundary cases cover missing runtime, Linux, unsupported OS and
// unknown platform; the runner stands in for the remote process boundary.
#[hegel::test]
fn docker_readiness_reports_endpoint_host_requirements(tc: hegel::TestCase) {
    use flotilla_discovery_testkit::DiscoveryMockRunner;
    use hegel::generators as gs;
    let case = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let runner = DiscoveryMockRunner::builder()
        .on_run("docker", &["--version"], if case == 0 { Err("runtime absent".into()) } else { Ok("Docker version".into()) })
        .on_run(
            "uname",
            &["-s"],
            match case {
                1 | 4 => Ok("Linux\n".into()),
                2 => Ok("Darwin\n".into()),
                _ => Err("platform probe failed".into()),
            },
        )
        .on_run(
            "docker",
            &["info", "--format", "{{json .}}"],
            if case == 4 { Err("runtime socket denied".into()) } else { Ok("{}".into()) },
        )
        .build();
    let provider =
        docker::DockerEnvironmentProvider::new(Arc::new(runner)).with_vessel_slice(Some(super::docker_vessels::DockerVessels::for_tests()));
    let readiness = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(provider.detect_readiness());
    match case {
        0 => assert!(matches!(readiness, ProviderReadiness::MissingTool { reason } if reason.contains("runtime absent"))),
        1 => assert_eq!(readiness, ProviderReadiness::Ready),
        2 => assert!(matches!(readiness, ProviderReadiness::MissingTool { reason } if reason.contains("Linux host"))),
        3 => assert!(matches!(readiness, ProviderReadiness::Unknown { reason } if reason.contains("platform probe failed"))),
        _ => assert!(matches!(readiness, ProviderReadiness::Unknown { reason } if reason.contains("runtime socket denied"))),
    }
}
