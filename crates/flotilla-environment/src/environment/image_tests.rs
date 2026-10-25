use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_resources::is_image_digest;
use hegel::generators as gs;

use super::{docker::DockerEnvironmentProvider, host_direct::HostDirectEnvironmentProvider, EnvironmentProvider};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;

fn digest(index: usize) -> String {
    format!("sha256:{index:064x}")
}

// Subprocess boundary stand-in. Enforces Docker's cache argv contract; two
// independent instances model separate cache endpoints.
#[derive(Default)]
struct DockerStandIn {
    held: Mutex<BTreeSet<String>>,
    repo_digests: Vec<String>,
    fail: bool,
    corrupt: bool,
    calls: Mutex<Vec<Vec<String>>>,
}
#[async_trait]
impl CommandRunner for DockerStandIn {
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        true
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
    }
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        assert_eq!(cmd, "docker");
        self.calls.lock().expect("calls").push(args.iter().map(|s| s.to_string()).collect());
        let private = args.first() == Some(&"--config");
        let args = if private {
            assert!(args[1].contains("flotilla-anonymous-"));
            &args[2..]
        } else {
            args
        };
        if self.fail {
            return Err("runtime unavailable".into());
        }
        let mut held = self.held.lock().expect("held");
        match args {
            ["image", "ls", "--no-trunc", "--digests", "--format", "{{.ID}} {{.Digest}}"] => Ok(held
                .iter()
                .cloned()
                .chain(self.repo_digests.iter().map(|r| r.rsplit_once('@').expect("digest").1.to_string()))
                .collect::<Vec<_>>()
                .join("\n")),
            ["image", "ls", "--no-trunc", "--digests", "--format", "{{.Repository}}@{{.Digest}}"] => Ok(self.repo_digests.join("\n")),
            ["pull", reference] => {
                assert!(private, "registry operations require isolated configuration");
                let (repo, digest) = reference.split_once('@').expect("pinned pull");
                assert_eq!(repo, "registry.example/team/image");
                assert!(is_image_digest(digest));
                held.insert(digest.into());
                Ok(String::new())
            }
            ["image", "inspect", "--format", "{{json .}}", id] => {
                let local = if id.contains('@') {
                    assert!(self.repo_digests.iter().any(|r| r == id));
                    held.iter().next().expect("local identity").as_str()
                } else {
                    assert!(held.contains(*id));
                    id
                };
                let attestations = if self.corrupt { vec![] } else { self.repo_digests.clone() };
                Ok(serde_json::json!({"Id": local, "RepoDigests": attestations}).to_string())
            }
            ["image", "rm", id] => {
                assert!(is_image_digest(id));
                if held.remove(*id) {
                    Ok(String::new())
                } else {
                    Err("image is absent".into())
                }
            }
            other => panic!("unexpected Docker invocation: {other:?}"),
        }
    }
}

// Every cache action uses exact digests and only the owning provider's endpoint.
// Generator spans empty caches, duplicate pulls, removals, observations, and
// interleaved actions on two provider instances on the same host.
#[hegel::test]
fn cache_actions_preserve_instance_isolation(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    let operations: Vec<_> = (0..count)
        .map(|_| {
            (
                tc.draw(gs::integers::<usize>().min_value(0).max_value(1)),
                tc.draw(gs::integers::<usize>().min_value(0).max_value(2)),
                tc.draw(gs::integers::<usize>().min_value(0).max_value(2)),
            )
        })
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let providers = [
            DockerEnvironmentProvider::new(Arc::new(DockerStandIn::default())),
            DockerEnvironmentProvider::new(Arc::new(DockerStandIn::default())),
        ];
        let mut expected = [BTreeSet::new(), BTreeSet::new()];
        for (index, op, id) in operations {
            let cache = providers[index].local_image_cache().expect("Docker cache");
            let id = digest(id);
            match op {
                0 => {
                    cache.pull(&format!("registry.example/team/image@{id}"), None).await.expect("pull");
                    expected[index].insert(id);
                }
                1 => {
                    let result = cache.remove(&id).await;
                    if expected[index].remove(&id) {
                        result.expect("remove held image");
                    } else {
                        assert!(result.is_err(), "Docker reports removal of absent content");
                    }
                }
                _ => {
                    assert_eq!(cache.has(&id).await.expect("has"), expected[index].contains(&id));
                }
            }
            for i in 0..2 {
                assert_eq!(providers[i].local_image_cache().expect("cache").inventory().await.expect("inventory"), expected[i]);
            }
        }
        let direct = HostDirectEnvironmentProvider::new(Arc::new(DockerStandIn::default()), Default::default());
        assert!(direct.local_image_cache().is_none());
    });
}

// Runtime errors remain errors; invalid/mutable cache references are rejected
// before a subprocess. Cacheless providers never advertise image capabilities.
#[tokio::test]
async fn cache_refuses_mutable_references_and_propagates_runtime_failures() {
    let runner = Arc::new(DockerStandIn { fail: true, ..Default::default() });
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let cache = provider.local_image_cache().expect("cache");
    for reference in ["", "latest", "registry.example/team/image:latest", "sha256:bad"] {
        assert!(cache.pull(reference, None).await.is_err());
        assert!(cache.inspect(reference).await.is_err());
        assert!(cache.remove(reference).await.is_err());
    }
    assert!(runner.calls.lock().expect("calls").is_empty());
    assert!(cache.inventory().await.is_err());
    assert!(cache.has(&digest(1)).await.is_err());
}

// Local-only commands preserve the provider endpoint without constructing an
// operation config; anonymous pulls still isolate ambient registry credentials.
#[tokio::test]
async fn local_cache_commands_do_not_allocate_registry_configuration() {
    let runner = Arc::new(DockerStandIn { held: Mutex::new(BTreeSet::from([digest(1)])), ..Default::default() });
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let cache = provider.local_image_cache().expect("cache");
    assert!(cache.inspect(&digest(1)).await.expect("inspect").is_some());
    cache.remove(&digest(1)).await.expect("remove");
    assert!(runner.calls.lock().expect("calls").iter().all(|args| args[0] == "image"));
    cache.pull(&format!("registry.example/team/image@{}", digest(2)), None).await.expect("pull");
    assert_eq!(runner.calls.lock().expect("calls").last().expect("pull")[0], "--config");
}

// A loaded config digest attests local content only. An exact repository and
// manifest digest must be present before inspect, and its returned attestation
// must agree; a sibling repository is absent even when content is shared.
#[tokio::test]
async fn cache_inspection_distinguishes_content_from_repository_attestations() {
    let repository = "registry.example/team/image";
    let reference = format!("{repository}@{}", digest(9));
    for attestations in [vec![], vec![reference.clone()]] {
        let runner = Arc::new(DockerStandIn {
            held: Mutex::new(BTreeSet::from([digest(1)])),
            repo_digests: attestations.clone(),
            ..Default::default()
        });
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let cache = provider.local_image_cache().expect("cache");
        let local = cache.inspect(&digest(1)).await.expect("local").expect("held");
        assert_eq!(local.local_image_id, digest(1));
        assert_eq!(local.registry_digest, None);
        assert!(cache.inspect(&format!("{repository}@{}", digest(1))).await.expect("loaded content is not repository evidence").is_none());
        let inspected = cache.inspect(&reference).await.expect("registry reference");
        assert_eq!(inspected.is_some(), !attestations.is_empty());
        if let Some(identity) = inspected {
            assert_eq!(identity.local_image_id, digest(1));
            assert_eq!(identity.registry_digest, Some(digest(9)));
        }
        assert!(cache.inspect(&format!("registry.example/other/image@{}", digest(9))).await.expect("other repository").is_none());
    }
    let provider = DockerEnvironmentProvider::new(Arc::new(DockerStandIn {
        held: Mutex::new(BTreeSet::from([digest(1)])),
        repo_digests: vec![reference.clone()],
        corrupt: true,
        ..Default::default()
    }));
    assert!(provider
        .local_image_cache()
        .expect("cache")
        .inspect(&reference)
        .await
        .expect_err("attestation changed")
        .contains("different registry digest"));
}

// Subprocess boundary stand-in for read-only Docker inspection. Any pulling,
// building, or mutation command fails the exact argv contract.
struct ReferenceProcess {
    local: bool,
    registry: bool,
    malformed: bool,
    launch_error: bool,
}

#[async_trait]
impl CommandRunner for ReferenceProcess {
    async fn run(&self, command: &str, args: &[&str], cwd: &Path, _: &ChannelLabel) -> Result<String, String> {
        assert_eq!(command, "docker");
        assert_eq!(args, ["manifest", "inspect", "crew:test"]);
        assert_eq!(cwd, Path::new("/"));
        self.registry.then(|| "manifest".into()).ok_or_else(|| "manifest unknown".into())
    }
    async fn run_output(&self, command: &str, args: &[&str], cwd: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(command, "docker");
        assert_eq!(args, ["image", "inspect", "crew:test"]);
        assert_eq!(cwd, Path::new("/"));
        if self.launch_error {
            return Err("cannot launch runtime".into());
        }
        let stdout = if self.malformed {
            "[]".into()
        } else {
            serde_json::json!([{"Id": digest(1), "RepoDigests": [format!("registry.example/crew@{}", digest(2))]}]).to_string()
        };
        Ok(CommandOutput { stdout, stderr: "image unavailable".into(), exit_code: Some(i32::from(!self.local)) })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        panic!("no discovery")
    }
}

// Validation can observe a mutable baseline without admitting or pulling it.
// Missing images, malformed success responses and launch failures are distinct;
// registry availability remains independent of the local cache observation.
#[hegel::test]
fn reference_observation_contract(tc: hegel::TestCase) {
    let local = tc.draw(gs::booleans());
    let registry = tc.draw(gs::booleans());
    let malformed = tc.draw(gs::booleans());
    let launch_error = tc.draw(gs::booleans());
    let provider = DockerEnvironmentProvider::new(Arc::new(ReferenceProcess { local, registry, malformed, launch_error }));
    let cache = provider.local_image_cache().expect("owned cache");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let observed = cache.inspect_reference("crew:test").await;
        if launch_error || (local && malformed) {
            assert!(observed.is_err());
        } else if local {
            let identity = observed.expect("observation").expect("local image");
            assert_eq!(identity.local_image_id, digest(1));
            assert_eq!(identity.registry_digest, Some(format!("registry.example/crew@{}", digest(2))));
        } else {
            assert!(observed.expect("absence").is_none());
        }
        assert_eq!(cache.registry_available("crew:test").await.is_ok(), registry);
        assert!(cache.inspect("crew:test").await.is_err(), "typed digest inspection still refuses tags");
    });
}
