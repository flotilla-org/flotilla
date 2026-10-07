//! Docker CLI adapter, always executed in the host's injected runner.
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_resources::{is_image_digest, PlacedImageIdentity};

use super::{ContainerProbe, ImageBuildOptions, ImageOperation, ImageStore, ProbeDirectory, ProbeNetwork};
use crate::providers::{
    discovery::detectors::generic::{parse_first_dotted_version, CommandDetector},
    ChannelLabel, CommandOutput, CommandRunner,
};

pub struct DockerImageStore {
    runner: Option<Arc<dyn CommandRunner>>,
}
impl DockerImageStore {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner: Some(runner) }
    }
    pub fn unavailable() -> Self {
        Self { runner: None }
    }
    fn runner(&self) -> Result<&dyn CommandRunner, String> {
        self.runner.as_deref().ok_or_else(|| "image store capability unavailable: no container CLI detected".into())
    }
    async fn output(&self, op: ImageOperation<'_>, args: &[&str], timeout: Duration) -> Result<CommandOutput, String> {
        let config = op.directory.to_str().ok_or("image auth path is not UTF-8")?;
        let mut private = vec!["--config", config];
        private.extend_from_slice(args);
        tokio::time::timeout(timeout, self.runner()?.run_output("docker", &private, op.context, &ChannelLabel::Default))
            .await
            .map_err(|_| format!("container process exceeded wall-clock deadline of {} seconds", timeout.as_secs()))?
    }
    async fn run(&self, op: ImageOperation<'_>, args: &[&str], timeout: Duration) -> Result<String, String> {
        let config = op.directory.to_str().ok_or("image auth path is not UTF-8")?;
        let mut private = vec!["--config", config];
        private.extend_from_slice(args);
        self.runner()?.run_with_timeout("docker", &private, op.context, &ChannelLabel::Default, timeout).await
    }
    pub async fn probe(&self, op: ImageOperation<'_>, probe: ContainerProbe<'_>) -> Result<CommandOutput, String> {
        let generated_name = format!("flotilla-probe-{}", uuid::Uuid::new_v4());
        let name = probe.name.unwrap_or(&generated_name);
        let cpu = probe.cpu.map(|value| value.to_string());
        let memory = probe.memory_bytes.map(|value| value.to_string());
        let pids = probe.pids_limit.map(|value| value.to_string());
        let mut args = vec!["run", "--rm", "--pull=never"];
        if matches!(probe.network, ProbeNetwork::Isolated) {
            args.push("--network=none");
        }
        if let Some(memory) = memory.as_deref() {
            args.extend(["--memory", memory]);
        }
        if let Some(pids) = pids.as_deref() {
            args.extend(["--pids-limit", pids]);
        }
        args.extend(["--name", name]);
        if let Some(cpu) = cpu.as_deref() {
            args.extend(["--cpus", cpu]);
        }
        if matches!(probe.directory, ProbeDirectory::Temporary) {
            args.extend(["--workdir", "/probe", "--tmpfs", "/probe"]);
        }
        args.extend(["--entrypoint", probe.command.first().ok_or("empty probe command")?.as_str(), probe.image]);
        args.extend(probe.command.iter().skip(1).map(String::as_str));
        let result = self.output(op, &args, probe.timeout).await;
        if result.is_err() {
            let _ = self.run(op, &["rm", "--force", name], Duration::from_secs(30)).await;
        }
        result
    }
}
#[async_trait]
impl ImageStore for DockerImageStore {
    async fn build(&self, op: ImageOperation<'_>, options: ImageBuildOptions<'_>) -> Result<CommandOutput, String> {
        let mut args = vec![
            "buildx".into(),
            "build".into(),
            "--builder".into(),
            "default".into(),
            "--load".into(),
            "--progress=plain".into(),
            "--platform".into(),
            options.platform.into(),
            "--tag".into(),
            options.tag.into(),
            "--file".into(),
            options.file.into(),
        ];
        for (name, value) in options.args {
            args.extend(["--build-arg".into(), format!("{name}={value}")]);
        }
        args.push(".".into());
        self.output(op, &args.iter().map(String::as_str).collect::<Vec<_>>(), options.timeout).await
    }
    async fn inspect(&self, op: ImageOperation<'_>, reference: &str) -> Result<Option<PlacedImageIdentity>, String> {
        let output = self.output(op, &["image", "inspect", "--format", "{{json .}}", reference], Duration::from_secs(30)).await?;
        if !output.success() {
            if output.stderr.contains("No such image:") {
                return Ok(None);
            }
            return Err(format!("image inspect failed: {}", output.stderr));
        }
        let output = output.stdout;
        let value: serde_json::Value = serde_json::from_str(&output).map_err(|error| error.to_string())?;
        let local_image_id =
            value["Id"].as_str().filter(|id| is_image_digest(id)).ok_or("image inspect has no valid local ID")?.to_string();
        let registry_digest = value["RepoDigests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .find(|digest| *digest == reference)
            .and_then(|reference| reference.rsplit_once('@'))
            .map(|(_, digest)| digest.to_string());
        Ok(Some(PlacedImageIdentity { local_image_id, registry_digest }))
    }
    async fn inventory(&self, op: ImageOperation<'_>) -> Result<BTreeSet<String>, String> {
        Ok(self
            .run(op, &["image", "ls", "--no-trunc", "--digests", "--format", "{{.ID}} {{.Digest}}"], Duration::from_secs(30))
            .await?
            .split_whitespace()
            .filter(|id| is_image_digest(id))
            .map(String::from)
            .collect())
    }
    async fn tag(&self, op: ImageOperation<'_>, image: &str, reference: &str) -> Result<(), String> {
        self.run(op, &["image", "tag", image, reference], Duration::from_secs(30)).await.map(|_| ())
    }
    async fn pull(&self, op: ImageOperation<'_>, reference: &str) -> Result<String, String> {
        self.run(op, &["pull", reference], Duration::from_secs(30 * 60)).await
    }
    async fn push(&self, op: ImageOperation<'_>, reference: &str) -> Result<String, String> {
        let output = self.run(op, &["push", reference], Duration::from_secs(30 * 60)).await?;
        output
            .split_once("digest:")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .filter(|digest| is_image_digest(digest))
            .map(String::from)
            .ok_or_else(|| "registry push did not report a manifest digest".into())
    }
    async fn save(&self, op: ImageOperation<'_>, image: &str, archive: &Path) -> Result<(), String> {
        self.run(
            op,
            &["image", "save", "--output", archive.to_str().ok_or("archive path is not UTF-8")?, image],
            Duration::from_secs(30 * 60),
        )
        .await
        .map(|_| ())
    }
    async fn load(&self, op: ImageOperation<'_>, archive: &Path) -> Result<(), String> {
        self.run(op, &["image", "load", "--input", archive.to_str().ok_or("archive path is not UTF-8")?], Duration::from_secs(30 * 60))
            .await
            .map(|_| ())
    }
    async fn remove(&self, op: ImageOperation<'_>, image: &str) -> Result<(), String> {
        if !is_image_digest(image) {
            return Err("image removal requires an immutable digest".into());
        }
        self.run(op, &["image", "rm", image], Duration::from_secs(30)).await.map(|_| ())
    }
    async fn login(&self, op: ImageOperation<'_>, registry: &str, username: &str, secret: &[u8]) -> Result<(), String> {
        let config = op.directory.to_str().ok_or("image auth path is not UTF-8")?;
        self.runner()?
            .run_with_input(
                "docker",
                &["--config", config, "login", "--username", username, "--password-stdin", registry],
                op.context,
                &ChannelLabel::Default,
                secret,
            )
            .await
            .map(|_| ())
    }
}

/// Discovery only asserts the binary; provider selection consumes that fact.
pub fn docker_binary_detector() -> CommandDetector {
    CommandDetector::new("docker", &["--version"], parse_first_dotted_version).with_resolved_path()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // The injected process stand-in records argv/stdin at the container CLI seam.
    #[derive(Default)]
    struct Processes {
        calls: Mutex<Vec<(Vec<String>, Vec<u8>)>>,
        inspect_failure: Option<CommandOutput>,
        inspect_error: bool,
        hang_probe: bool,
    }
    #[async_trait]
    impl CommandRunner for Processes {
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "docker");
            self.calls.lock().expect("calls").push((args.iter().map(|arg| arg.to_string()).collect(), vec![]));
            if args.get(2) == Some(&"push") {
                return Ok(format!("digest: sha256:{} size: 123", "b".repeat(64)));
            }
            match args.get(3).copied() {
                Some("inspect") => Ok(serde_json::json!({"Id": format!("sha256:{}", "a".repeat(64)), "RepoDigests": [format!("registry.test/images@sha256:{}", "b".repeat(64))]}).to_string()),
                Some("ls") => Ok(format!("sha256:{} sha256:{} <none>", "a".repeat(64), "b".repeat(64))),
                _ => Ok(String::new()),
            }
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            if args.get(2) == Some(&"run") && self.hang_probe {
                self.calls.lock().expect("calls").push((args.iter().map(|arg| arg.to_string()).collect(), vec![]));
                std::future::pending::<()>().await;
            }
            if args.get(3) == Some(&"inspect") {
                if self.inspect_error {
                    return Err("daemon offline".into());
                }
                if let Some(output) = &self.inspect_failure {
                    return Ok(CommandOutput { stdout: output.stdout.clone(), stderr: output.stderr.clone(), exit_code: output.exit_code });
                }
            }
            self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
        }
        async fn run_with_input(
            &self,
            cmd: &str,
            args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            input: &[u8],
        ) -> Result<String, String> {
            assert_eq!(cmd, "docker");
            self.calls.lock().expect("calls").push((args.iter().map(|arg| arg.to_string()).collect(), input.to_vec()));
            Ok(String::new())
        }
    }

    // All image operations use the admitted private config; secret material
    // goes only to login stdin. Immutable inspection separates local/manifest IDs.
    #[tokio::test]
    async fn operations_keep_private_auth_and_distinct_identities() {
        let processes = Arc::new(Processes::default());
        let images = DockerImageStore::new(processes.clone());
        let op = ImageOperation { directory: Path::new("/private/config"), context: Path::new("/context") };
        let id = format!("sha256:{}", "a".repeat(64));
        let reference = format!("registry.test/images@sha256:{}", "b".repeat(64));
        images.login(op, "registry.test", "user", b"secret").await.expect("login");
        images.pull(op, &reference).await.expect("pull");
        assert_eq!(images.push(op, "registry.test/images:temporary").await.expect("push"), format!("sha256:{}", "b".repeat(64)));
        let identity = images.inspect(op, &reference).await.expect("inspect").expect("present");
        assert_eq!(identity.local_image_id, id);
        assert_eq!(identity.registry_digest, Some(format!("sha256:{}", "b".repeat(64))));
        assert_eq!(images.inspect(op, &id).await.expect("local inspect").expect("present").registry_digest, None);
        assert_eq!(images.inventory(op).await.expect("inventory").len(), 2);
        images.tag(op, &id, "temporary").await.expect("tag");
        images.save(op, &id, Path::new("/private/archive.tar")).await.expect("save");
        images.load(op, Path::new("/private/archive.tar")).await.expect("load");
        images.remove(op, &id).await.expect("remove");
        let calls = processes.calls.lock().expect("calls");
        assert_eq!(calls.len(), 10);
        assert_eq!(calls[0].1, b"secret");
        assert!(calls
            .iter()
            .all(|(args, _)| args[..2] == ["--config", "/private/config"] && !args.iter().any(|arg| arg.contains("secret"))));
        assert!(calls.iter().skip(1).all(|(_, input)| input.is_empty()));
        assert_eq!(calls[7].0[2..], ["image", "save", "--output", "/private/archive.tar", &id]);
        assert_eq!(calls[8].0[2..], ["image", "load", "--input", "/private/archive.tar"]);
    }

    #[tokio::test]
    async fn inspection_distinguishes_absence_from_failure_and_timeout_cleans_probes() {
        let op = ImageOperation { directory: Path::new("/private"), context: Path::new("/") };
        for (stderr, absent) in [("No such image: missing", true), ("daemon unavailable", false), ("permission denied", false)] {
            let images = DockerImageStore::new(Arc::new(Processes {
                inspect_failure: Some(CommandOutput { stdout: String::new(), stderr: stderr.into(), exit_code: Some(1) }),
                ..Default::default()
            }));
            let result = images.inspect(op, "missing").await;
            if absent {
                assert_eq!(result.expect("definite absence"), None);
            } else {
                assert!(result.is_err());
            }
        }
        let images = DockerImageStore::new(Arc::new(Processes { inspect_error: true, ..Default::default() }));
        assert!(images.inspect(op, "missing").await.is_err());
        for name in [Some("named-probe"), None] {
            let processes = Arc::new(Processes { hang_probe: true, ..Default::default() });
            let images = DockerImageStore::new(processes.clone());
            let command = vec!["true".to_string()];
            assert!(images
                .probe(
                    op,
                    ContainerProbe::builder().image("image").maybe_name(name).command(&command).timeout(Duration::from_millis(1)).build()
                )
                .await
                .is_err());
            let calls = processes.calls.lock().expect("calls");
            let probe_name = calls[0].0.windows(2).find(|pair| pair[0] == "--name").expect("name")[1].clone();
            assert_eq!(calls[1].0[2..], ["rm", "--force", &probe_name]);
            if let Some(name) = name {
                assert_eq!(probe_name, name);
            }
        }
    }

    // Mutable or malformed IDs cannot select an image for removal. The explicit
    // generator spans lengths on both sides of the 64-character SHA256 boundary.
    #[hegel::test]
    fn remove_requires_digest_identity(tc: hegel::TestCase) {
        let length = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(66));
        let immutable = tc.draw(hegel::generators::booleans());
        let image = if immutable { format!("sha256:{}", "a".repeat(length)) } else { "mutable:tag".into() };
        let processes = Arc::new(Processes::default());
        let images = DockerImageStore::new(processes.clone());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let result =
            runtime.block_on(images.remove(ImageOperation { directory: Path::new("/private/config"), context: Path::new("/") }, &image));
        assert_eq!(result.is_ok(), immutable && length == 64);
        assert_eq!(processes.calls.lock().expect("calls").len(), usize::from(immutable && length == 64));
    }
}
