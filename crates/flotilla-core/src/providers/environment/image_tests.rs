use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_resources::is_image_digest;
use hegel::generators as gs;

use super::{
    docker::DockerEnvironmentProvider,
    docker_image::BuildxImageBuilder,
    host_direct::HostDirectEnvironmentProvider,
    image::{BuiltImage, ImageComposition},
    EnvironmentProvider, ImageBuilder,
};
use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};

fn digest(index: usize) -> String {
    format!("sha256:{index:064x}")
}

// Subprocess boundary stand-in. Enforces Docker's argv and Buildx export
// contract; two independent instances model separate cache endpoints.
#[derive(Default)]
struct DockerStandIn {
    held: Mutex<BTreeSet<String>>,
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
        assert_eq!(args[0], "--config");
        assert!(args[1].contains("flotilla-anonymous-"));
        self.calls.lock().expect("calls").push(args.iter().map(|s| s.to_string()).collect());
        if self.fail {
            return Err("runtime unavailable".into());
        }
        let mut held = self.held.lock().expect("held");
        match &args[2..] {
            ["image", "ls", "--no-trunc", "--digests", "--format", "{{.ID}} {{.Digest}}"] => {
                Ok(held.iter().cloned().collect::<Vec<_>>().join("\n"))
            }
            ["pull", reference] => {
                let (repo, digest) = reference.split_once('@').expect("pinned pull");
                assert_eq!(repo, "registry.example/team/image");
                assert!(is_image_digest(digest));
                held.insert(digest.into());
                Ok(String::new())
            }
            ["image", "inspect", "--format", "{{json .}}", id] => {
                assert!(held.contains(*id));
                Ok(serde_json::json!({"Id": id, "RepoDigests": []}).to_string())
            }
            ["image", "rm", id] => {
                assert!(is_image_digest(id));
                if held.remove(*id) {
                    Ok(String::new())
                } else {
                    Err("image is absent".into())
                }
            }
            ["image", "load", "--input", path] => {
                let id = std::fs::read_to_string(path).expect("archive");
                assert!(is_image_digest(&id));
                held.insert(id);
                Ok(String::new())
            }
            ["image", "tag", id, tag] => {
                assert!(held.contains(*id));
                assert!(tag.starts_with("registry.example/team/image:flotilla-"));
                Ok(String::new())
            }
            ["push", tag] => {
                assert!(tag.starts_with("registry.example/team/image:flotilla-"));
                Ok(format!("digest: {} size: 123", digest(9)))
            }
            ["buildx", "build", rest @ ..] => {
                assert!(rest.windows(2).any(|p| p == ["--platform", "linux/amd64"]));
                assert!(rest.windows(2).any(|p| p == ["--file", "Dockerfile"]));
                assert!(rest.windows(2).any(|p| p == ["--build-arg", "BASE=parent"]));
                assert_eq!(rest.last(), Some(&"."));
                let output = rest.windows(2).find(|p| p[0] == "--output").expect("archive exporter")[1];
                std::fs::write(output.strip_prefix("type=docker,dest=").expect("Docker archive"), digest(1)).expect("archive");
                let metadata = rest.windows(2).find(|p| p[0] == "--metadata-file").expect("metadata file")[1];
                std::fs::write(
                    metadata,
                    serde_json::json!({"containerimage.config.digest": if self.corrupt { "invalid".to_string() } else { digest(1) }})
                        .to_string(),
                )
                .expect("metadata");
                Ok(String::new())
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

// A build emits an immutable digest before loading a specifically named cache
// or publishing a registry digest; the target cache is verified after import.
#[tokio::test]
async fn builder_contract_build_load_and_push() {
    let directory = tempfile::tempdir().expect("directory");
    let runner = Arc::new(DockerStandIn::default());
    let builder: Arc<dyn ImageBuilder> = Arc::new(BuildxImageBuilder::new(runner.clone()));
    let args = BTreeMap::from([("BASE".into(), "parent".into())]);
    let image = builder
        .build(
            ImageComposition { context: directory.path(), fragment: "Dockerfile", architecture: "amd64", args: &args },
            &directory.path().join("image.tar"),
            None,
        )
        .await
        .expect("build");
    assert_eq!(image.digest, digest(1));
    assert!(!directory.path().join("image.metadata.json").exists());
    let target = DockerEnvironmentProvider::new(Arc::new(DockerStandIn::default()));
    let cache = target.local_image_cache().expect("target");
    assert!(!cache.has(&image.digest).await.expect("absent"));
    assert!(builder.load(&image, "", cache).await.is_err());
    builder.load(&image, "cache-b", cache).await.expect("load");
    assert!(cache.has(&image.digest).await.expect("loaded"));
    assert_eq!(
        builder.push(&image, "registry.example/team/image", None).await.expect("push"),
        format!("registry.example/team/image@{}", digest(9))
    );
    let bad = BuiltImage { digest: digest(2), archive: image.archive };
    assert!(builder.load(&bad, "cache-b", cache).await.expect_err("mismatch").contains("does not match"));
}

// Invalid Buildx metadata is never admitted as immutable build evidence.
#[tokio::test]
async fn builder_contract_refuses_invalid_digest() {
    let directory = tempfile::tempdir().expect("directory");
    let builder = BuildxImageBuilder::new(Arc::new(DockerStandIn { corrupt: true, ..Default::default() }));
    let args = BTreeMap::from([("BASE".into(), "parent".into())]);
    assert!(builder
        .build(
            ImageComposition { context: directory.path(), fragment: "Dockerfile", architecture: "amd64", args: &args },
            &directory.path().join("image.tar"),
            None
        )
        .await
        .expect_err("invalid digest")
        .contains("valid config digest"));
}
