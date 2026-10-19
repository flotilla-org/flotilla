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

// Process-boundary fake: a small Docker engine with persistent inventory. It
// refuses unexpected commands rather than returning success for arbitrary input.
#[derive(Default)]
struct DockerEngine {
    inventory: Mutex<BTreeMap<String, String>>,
    unavailable: AtomicBool,
}
#[async_trait]
impl CommandRunner for DockerEngine {
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        if cmd == "cat" {
            return Err("no cgroup sample".into());
        }
        assert_eq!(cmd, "docker");
        if self.unavailable.load(Ordering::Acquire) {
            return Err("provider API unavailable".into());
        }
        let mut inventory = self.inventory.lock().unwrap();
        match args {
            ["image", "inspect", ..] => Ok("[]".into()),
            ["info", "--format", "{{json .}}"] => Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into()),
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
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        Ok(CommandOutput { stdout: self.run(cmd, args, cwd, label).await?, stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        false
    }
}
fn immutable_id(id: &str) -> String {
    format!("{:0>64}", id.strip_prefix("env-").unwrap())
}

fn providers() -> Vec<(Arc<dyn EnvironmentProvider>, flotilla_resources::EnvironmentSpec)> {
    let image_spec =
        legacy_environment_spec(&EnvironmentSpec { image: ImageSource::Registry(DIGEST.into()), token_env_vars: vec![] }).unwrap();
    vec![
        (Arc::new(docker::DockerEnvironmentProvider::new(Arc::new(DockerEngine::default()))), image_spec.clone()),
        (Arc::new(dummy::DummyEnvironmentProvider::new(Default::default())), image_spec),
        (
            Arc::new(host_direct::HostDirectEnvironmentProvider::new(Arc::new(DockerEngine::default()), Default::default())),
            flotilla_resources::EnvironmentSpec {
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
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
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
// The three cases exhaust the installed adapters; the public capability data
// agrees with optional accessors and the dummy has its own descriptive class.
#[tokio::test]
async fn provider_instances_and_declarations() {
    for ((provider, spec), (other, _)) in providers().into_iter().zip(providers()) {
        let prepared = provider.prepare(&spec, &Default::default()).await.unwrap();
        assert!(other.provision(EnvironmentId::new("wrong-owner"), &prepared, Default::default()).await.is_err());
        let declaration = provider.capabilities();
        assert!(!declaration.workspace_root.as_os_str().is_empty());
    }
    assert_eq!(providers()[1].0.kind(), ProviderKind::new(EnvironmentClass::Dummy, "dummy"));
}

// A provider error means Unknown, never Gone. The same handle recovers
// when the injected process boundary becomes available again.
#[tokio::test(start_paused = true)]
async fn observation_recovers_after_provider_error() {
    let engine = Arc::new(DockerEngine::default());
    let provider = Arc::new(docker::DockerEnvironmentProvider::new(engine.clone()));
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
}

// Advertisements are additive data: missing fields decode to their declared
// defaults. Generate every top-level and nested capability field omission.
#[hegel::test]
fn advertisement_fields_default_when_omitted(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let (provider, _) = providers().remove(tc.draw(gs::integers::<usize>().min_value(0).max_value(2)));
    let advertisement =
        ProviderAdvertisement { instance: "configured".into(), kind: provider.kind(), capabilities: provider.capabilities() };
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
