use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, ImageSource};

use super::{
    contained_daemon_socket_path, contract_tests::LocalImageTags, docker::DockerEnvironmentProvider, runner::DockerEnvironmentRunner,
    CreateOpts, EnvironmentProvider, EnvironmentTool, EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind,
    EnvironmentVariableUpdate, ImagePullPolicy, ProvisionedMount, ProvisionedMountMode,
};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;

// Boundary double: admitted material is lowered to a virtual tool config.
struct TestRegistryMaterial(PathBuf);
impl super::registry_auth::RegistryAuthMaterial for TestRegistryMaterial {
    fn config(&self) -> Result<super::registry_auth::RegistryConfig, String> {
        Ok(super::registry_auth::RegistryConfig::new(self.0.clone(), ()))
    }
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
    }
}
fn test_registry_auth(path: &str) -> super::PreparedEnvironmentAuth {
    Some(super::RegistryAuth::new(
        "registry.example/crew".into(),
        flotilla_resources::HostImageAction::ImagePull,
        Arc::new(TestRegistryMaterial(path.into())),
    ))
}

fn test_daemon_tool(socket_path: impl Into<PathBuf>) -> EnvironmentTool {
    let socket_path = socket_path.into();
    let environment_socket_path = contained_daemon_socket_path(&socket_path);
    EnvironmentTool::new("flotilla", "/usr/local/bin/flotilla")
        .with_asset(EnvironmentToolAsset::new(
            socket_path,
            environment_socket_path.clone(),
            EnvironmentToolAssetKind::UnixSocket,
            EnvironmentToolAssetAccess::SharedWritable,
            "the daemon socket",
        ))
        .with_environment(EnvironmentVariableUpdate::set(
            "FLOTILLA_DAEMON_SOCKET",
            environment_socket_path.to_string_lossy(),
            "the daemon socket",
        ))
}

/// A mock CommandRunner that records all (cmd, args, cwd) tuples passed to run/run_output.
struct RecordingRunner {
    calls: Mutex<Vec<(String, Vec<String>, PathBuf)>>,
    result: Result<String, String>,
    image_environment: Mutex<VecDeque<Result<String, String>>>,
    memory_info: Option<Result<String, String>>,
    slice_show: Result<String, String>,
    tags: LocalImageTags,
    dockerfiles: Mutex<Vec<String>>,
}

impl RecordingRunner {
    fn new_ok(output: &str) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            result: Ok(output.to_string()),
            image_environment: Mutex::new(VecDeque::new()),
            memory_info: None,
            slice_show: Err("systemctl not expected".into()),
            tags: Default::default(),
            dockerfiles: Mutex::new(vec![]),
        }
    }

    fn new_err(msg: &str) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            result: Err(msg.to_string()),
            image_environment: Mutex::new(VecDeque::new()),
            memory_info: None,
            slice_show: Err("systemctl not expected".into()),
            tags: Default::default(),
            dockerfiles: Mutex::new(vec![]),
        }
    }

    /// Answers the next `docker image inspect --format '{{json .Config.Env}}'`
    /// with `output`. Repeated calls queue successive answers; the last one
    /// answers every later inspection.
    fn with_image_environment(self, output: Result<&str, &str>) -> Self {
        self.image_environment.lock().expect("image environment mutex").push_back(output.map(str::to_string).map_err(str::to_string));
        self
    }

    fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
        self.calls
            .lock()
            .expect("calls mutex")
            .iter()
            .cloned()
            .map(|(cmd, mut args, cwd)| {
                if args.first().is_some_and(|a| a == "--config") && args.get(1).is_some_and(|a| a.contains("flotilla-anonymous-")) {
                    args.drain(..2);
                }
                (cmd, args, cwd)
            })
            .collect()
    }
}

#[async_trait]
impl CommandRunner for RecordingRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        if cmd == "docker" && args.starts_with(&["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}"]) {
            return Ok("linux/amd64".into());
        }
        // Host capacity probe fixture, independent of the command under test.
        if cmd == "docker" && args == ["info", "--format", "{{json .}}"] {
            return self
                .memory_info
                .clone()
                .unwrap_or_else(|| Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into()));
        }
        self.calls.lock().expect("calls mutex").push((cmd.to_string(), args.iter().map(|a| a.to_string()).collect(), cwd.to_path_buf()));
        if cmd == "systemctl" {
            return self.slice_show.clone();
        }
        if let ("docker", ["tag", id, tag]) = (cmd, args) {
            return self.tags.tag(id, tag);
        }
        if cmd == "docker" && args.starts_with(&["inspect", "--format", "{{.Image}}"]) {
            return Ok("sha256:test-image-digest\n".to_string());
        }
        if cmd == "docker" && args.starts_with(&["image", "inspect", "--format", "{{json .Config.Env}}"]) {
            let mut image_environment = self.image_environment.lock().expect("image environment mutex");
            let answer = if image_environment.len() > 1 { image_environment.pop_front() } else { image_environment.front().cloned() };
            if let Some(answer) = answer {
                return answer;
            }
        }
        self.result.clone()
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        assert_eq!(cmd, "docker");
        assert_eq!(args, ["build", "--platform", "linux/amd64", "--quiet", "--network=none", "-"]);
        let dockerfile = std::str::from_utf8(input).unwrap();
        assert!(dockerfile.contains("COPY --from=tools /opt/drydock /opt/drydock"));
        self.dockerfiles.lock().unwrap().push(dockerfile.into());
        self.calls.lock().unwrap().push((cmd.into(), args.iter().map(|s| s.to_string()).collect(), cwd.into()));
        // The derived image keeps the base's identity in this double.
        Ok(self.tags.resolve_build(dockerfile)?.unwrap_or_else(|| format!("sha256:{}", "a".repeat(64))))
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, exit_code: Some(1) }),
        }
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

#[tokio::test]
async fn run_wraps_with_docker_exec() {
    let inner = Arc::new(RecordingRunner::new_ok(""));
    let env_runner = DockerEnvironmentRunner::new("test-container".to_string(), inner.clone());
    let label = ChannelLabel::Default;

    env_runner.run("git", &["status"], Path::new("/workspace"), &label).await.ok();

    let calls = inner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, cwd) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args, &["exec", "-w", "/workspace", "test-container", "git", "status"]);
    assert_eq!(cwd, Path::new("/"));
}

#[tokio::test]
async fn run_output_wraps_with_docker_exec() {
    let inner = Arc::new(RecordingRunner::new_ok("output"));
    let env_runner = DockerEnvironmentRunner::new("test-container".to_string(), inner.clone());
    let label = ChannelLabel::Default;

    env_runner.run_output("git", &["status"], Path::new("/workspace"), &label).await.ok();

    let calls = inner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, cwd) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args, &["exec", "-w", "/workspace", "test-container", "git", "status"]);
    assert_eq!(cwd, Path::new("/"));
}

#[tokio::test]
async fn exists_uses_run_with_which() {
    let inner = Arc::new(RecordingRunner::new_ok(""));
    let env_runner = DockerEnvironmentRunner::new("test-container".to_string(), inner.clone());

    let result = env_runner.exists("cleat", &[]).await;

    assert!(result);
    let calls = inner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, cwd) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args, &["exec", "test-container", "which", "cleat"]);
    assert_eq!(cwd, Path::new("/"));
}

#[tokio::test]
async fn exists_returns_false_on_failure() {
    let inner = Arc::new(RecordingRunner::new_err("not found"));
    let env_runner = DockerEnvironmentRunner::new("test-container".to_string(), inner.clone());

    let result = env_runner.exists("cleat", &[]).await;

    assert!(!result);
}

// ---------------------------------------------------------------------------
// Multi-response mock runner for sequential command scenarios
// ---------------------------------------------------------------------------

/// A mock CommandRunner that returns successive responses from a queue.
/// Records all calls for later assertion.
struct QueuedRunner {
    calls: Mutex<Vec<(String, Vec<String>, PathBuf)>>,
    responses: Mutex<VecDeque<Result<String, String>>>,
    tags: LocalImageTags,
}

impl QueuedRunner {
    fn new(responses: impl IntoIterator<Item = Result<String, String>>) -> Self {
        Self { calls: Mutex::new(vec![]), responses: Mutex::new(responses.into_iter().collect()), tags: Default::default() }
    }

    fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
        self.calls
            .lock()
            .expect("calls mutex")
            .iter()
            .cloned()
            .map(|(cmd, mut args, cwd)| {
                if args.first().is_some_and(|a| a == "--config") && args.get(1).is_some_and(|a| a.contains("flotilla-anonymous-")) {
                    args.drain(..2);
                }
                (cmd, args, cwd)
            })
            .collect()
    }
}

#[async_trait]
impl CommandRunner for QueuedRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        if cmd == "docker" && args.starts_with(&["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}"]) {
            return Ok("linux/amd64".into());
        }
        // Host capacity probe fixture, independent of the command under test.
        if args.starts_with(&["exec", "--user", "0"])
            || matches!(args, ["exec", _, "/opt/drydock/drydock-agent", "--supervise", "--socket", "/run/drydock/agent.sock"])
        {
            return Ok(String::new());
        }

        if cmd == "docker" && args == ["info", "--format", "{{json .}}"] {
            return Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into());
        }
        if cmd == "docker" && args.starts_with(&["image", "inspect", "--format", "{{.Os}}/{{.Architecture}}"]) {
            return Ok("linux/amd64".into());
        }
        self.calls.lock().expect("calls mutex").push((cmd.to_string(), args.iter().map(|a| a.to_string()).collect(), cwd.to_path_buf()));
        if let ("docker", ["tag", id, tag]) = (cmd, args) {
            return self.tags.tag(id, tag);
        }
        let mut queue = self.responses.lock().expect("responses mutex");
        queue.pop_front().unwrap_or(Err("no more responses".into()))
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        assert_eq!(cmd, "docker");
        assert_eq!(args, ["build", "--platform", "linux/amd64", "--quiet", "--network=none", "-"]);
        let dockerfile = std::str::from_utf8(input).unwrap();
        assert!(dockerfile.contains("COPY --from=tools /opt/drydock /opt/drydock"));
        self.calls.lock().unwrap().push((cmd.into(), args.iter().map(|s| s.to_string()).collect(), cwd.into()));
        // The derived image keeps the base's identity in this double.
        Ok(self.tags.resolve_build(dockerfile)?.unwrap_or_else(|| format!("sha256:{}", "a".repeat(64))))
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, exit_code: Some(1) }),
        }
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// DockerEnvironmentProvider tests
// ---------------------------------------------------------------------------

// Providers never build. Generate both kinds and duplicate/ambiguous specs;
// preparation either validates the selected kind or refuses before execution.
#[hegel::test]
fn environment_specs_select_exactly_one_kind(tc: hegel::TestCase) {
    use super::EnvironmentInput;
    use hegel::generators as gs;
    let host = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let docker = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let mut spec = pinned_spec();
    if !docker {
        spec.docker = None;
    }
    spec.host_direct =
        host.then(|| flotilla_resources::HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/".into() });
    assert_eq!(EnvironmentInput::of(&spec).is_ok(), host != docker);
}

fn pinned_spec() -> flotilla_resources::EnvironmentSpec {
    super::legacy_environment_spec(&EnvironmentSpec {
        image: ImageSource::Registry(format!("sha256:{}", "a".repeat(64))),
        token_env_vars: Vec::new(),
    })
    .expect("registry spec")
}

// The subprocess double represents the Docker CLI. A held digest prepares
// without pulling/rebuilding the base; absent local-only digests wait on ImageBuild.
#[tokio::test]
#[cfg(unix)]
async fn preparation_adds_only_drydock_tools_to_existing_digest() {
    for present in [true, false] {
        let runner = Arc::new(RecordingRunner::new_ok("present"));
        let missing = Arc::new(RecordingRunner::new_err("absent"));
        let selected = if present { runner } else { missing };
        let provider = super::contract_tests::docker_provider(selected.clone());
        assert_eq!(provider.prepare(&pinned_spec(), &Default::default()).await.is_ok(), present);
        assert_eq!(selected.calls().len(), if present { 3 } else { 1 });
        assert_eq!(selected.calls()[0].1[0..2], ["image", "inspect"]);
    }
}

// #3756 on feta: BuildKit resolves `FROM sha256:<id>` as Docker Hub's `sha256`
// repository. The double refuses that, so a bare local ID must be tagged first.
#[tokio::test]
#[cfg(unix)]
async fn derived_build_names_a_bare_local_id_before_building_from_it() {
    let id = format!("sha256:{}", "a".repeat(64));
    let tag = format!("flotilla-agent-base:{}", "a".repeat(64));
    let runner = Arc::new(RecordingRunner::new_ok("present"));
    let provider = super::contract_tests::docker_provider(runner.clone());
    provider.prepare(&pinned_spec(), &Default::default()).await.expect("prepare local digest");
    let calls: Vec<_> = runner.calls().into_iter().map(|(_, args, _)| args).collect();
    assert_eq!(calls[1], ["tag", id.as_str(), tag.as_str()]);
    assert_eq!(calls[2], ["build", "--platform", "linux/amd64", "--quiet", "--network=none", "-"]);
    let dockerfiles = runner.dockerfiles.lock().unwrap().clone();
    assert_eq!(dockerfiles.len(), 1);
    assert!(dockerfiles[0].ends_with(&format!("\nFROM --platform=linux/amd64 {tag}\nCOPY --from=tools /opt/drydock /opt/drydock\n")));
}

// A registry digest reference already resolves in BuildKit; it is built from
// exactly as admitted, and no local tag is created for it.
#[tokio::test]
#[cfg(unix)]
async fn derived_build_uses_a_registry_digest_reference_unchanged() {
    let reference = format!("registry.example/crew@sha256:{}", "a".repeat(64));
    let mut spec = pinned_spec();
    spec.docker.as_mut().expect("docker").image = reference.clone();
    let runner = Arc::new(RecordingRunner::new_ok("present"));
    let provider = super::contract_tests::docker_provider(runner.clone());
    provider.prepare(&spec, &Default::default()).await.expect("prepare registry digest");
    assert!(runner.calls().iter().all(|(_, args, _)| args.first().is_none_or(|arg| arg != "tag")));
    let dockerfiles = runner.dockerfiles.lock().unwrap().clone();
    assert_eq!(dockerfiles.len(), 1);
    assert!(dockerfiles[0].ends_with(&format!("\nFROM --platform=linux/amd64 {reference}\nCOPY --from=tools /opt/drydock /opt/drydock\n")));
}

// Host adoption returns the injected runner and never starts or stops the host.
// A preparation cannot cross instances, even when they implement the same kind.
#[tokio::test]
async fn host_adoption_is_idempotent_and_preparation_is_instance_scoped() {
    use super::host_direct::HostDirectEnvironmentProvider;
    let runner = Arc::new(RecordingRunner::new_ok(""));
    let provider = HostDirectEnvironmentProvider::new(runner.clone(), Default::default());
    let other = HostDirectEnvironmentProvider::new(runner.clone(), Default::default());
    let spec = flotilla_resources::EnvironmentSpec {
        cloud_sandbox: None,
        provider_address: None,
        host_direct: Some(flotilla_resources::HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/".into() }),
        docker: None,
    };
    let prepared = provider.prepare(&spec, &Default::default()).await.expect("prepare host");
    for opts in [
        super::ProvisionOpts { tokens: vec![("TOKEN".into(), "secret".into())], ..Default::default() },
        super::ProvisionOpts { tools: vec![test_daemon_tool("/socket")], ..Default::default() },
        super::ProvisionOpts {
            provisioned_mounts: vec![ProvisionedMount::new("/host", "/guest", ProvisionedMountMode::Ro)],
            ..Default::default()
        },
        super::ProvisionOpts { cpu_limit: Some(1), ..Default::default() },
    ] {
        assert!(provider.provision(EnvironmentId::new("unsupported"), &prepared, opts).await.is_err());
        assert!(provider.list().await.expect("no adoption on rejected opts").is_empty());
    }
    let id = EnvironmentId::new("adopted");
    assert!(other.provision(id.clone(), &prepared, Default::default()).await.is_err());
    let first = provider.provision(id.clone(), &prepared, Default::default()).await.expect("adopt");
    let second = provider.provision(id.clone(), &prepared, Default::default()).await.expect("adopt twice");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(provider.inspect(&id).await.expect("inspect").is_some());
    assert_eq!(provider.list().await.expect("list").len(), 1);
    provider.destroy(id.as_str()).await.expect("detach");
    provider.destroy(id.as_str()).await.expect("detach twice");
    assert!(provider.list().await.expect("list").is_empty());
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn create_returns_handle() {
    use flotilla_protocol::ImageId;
    let runner = Arc::new(QueuedRunner::new([Ok("container-id-123".into()), Ok("sha256:8c7f4e5d6a1b\n".into())]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![("GITHUB_TOKEN".into(), "ghp_secret".into())],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let id = EnvironmentId::new("test-env-1");
    let result = provider.create(id, &image, opts).await;

    assert!(result.is_ok(), "create should succeed");
    let handle = result.unwrap();

    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    let (cmd, args, _) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "run");
    assert!(args.contains(&"-d".to_string()), "should detach");
    assert!(args.contains(&"--init".to_string()), "docker should reap orphaned processes; args: {args:?}");
    assert!(args.contains(&"--name".to_string()), "should set name");
    assert!(args.contains(&"--label".to_string()), "should set label");
    assert!(args.contains(&"sleep".to_string()), "should run sleep infinity");
    assert!(args.contains(&"infinity".to_string()), "should run sleep infinity");

    // Label should match environment id
    let label_idx = args.iter().position(|a| a == "--label").expect("--label flag");
    let label_val = &args[label_idx + 1];
    assert!(label_val.starts_with("flotilla.environment="), "label should be flotilla.environment=<id>");

    // Environment ID in handle should match label value
    let expected_id = label_val.strip_prefix("flotilla.environment=").unwrap();
    assert_eq!(handle.id().as_str(), expected_id);
    assert_eq!(handle.image().as_str(), "ubuntu:22.04");
    assert_eq!(handle.local_image_id(), Some("sha256:8c7f4e5d6a1b"));

    let (inspect_cmd, inspect_args, _) = &calls[1];
    assert_eq!(inspect_cmd, "docker");
    assert_eq!(inspect_args, &["inspect", "--format", "{{.Image}}", "flotilla-env-test-env-1"]);

    // Token env var should be present
    assert!(args.iter().any(|a| a.starts_with("GITHUB_TOKEN=")), "token env var should be passed");
}

#[cfg(unix)]
#[tokio::test]
async fn create_runs_container_as_the_host_user() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: Vec::new(),
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: Vec::new(),
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("host-user"), &image, opts).await.expect("create environment as host user");

    let calls = runner.calls();
    let (_, args, _) = &calls[0];
    // SAFETY: getuid and getgid are side-effect-free process identity queries.
    let host_user = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };
    assert!(
        args.windows(2).any(|pair| pair == ["--user", host_user.as_str()]),
        "docker container should run as the host uid:gid; args: {args:?}",
    );
}

#[tokio::test]
async fn create_removes_container_when_local_image_id_cannot_be_resolved() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(QueuedRunner::new([Ok("container-id-123".into()), Ok("not-a-digest".into()), Err("docker rm failed".into())]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("registry.example/crew:latest");
    let opts = CreateOpts {
        tokens: Vec::new(),
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: Vec::new(),
        image_pull_policy: ImagePullPolicy::Always,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let error = match provider.create(EnvironmentId::new("invalid-digest"), &image, opts).await {
        Ok(_) => panic!("invalid digest should reject the environment"),
        Err(error) => error,
    };

    assert!(error.contains("invalid image digest"), "{error}");
    assert!(error.contains("additionally failed to remove container flotilla-env-invalid-digest: docker rm failed"), "{error}");
    let calls = runner.calls();
    assert_eq!(calls[2].0, "docker");
    assert_eq!(calls[2].1, ["rm", "-f", "container-id-123"]);
}

// Docker crews retain pull and memory policies while sharing CPU without a quota.
#[tokio::test]
async fn create_translates_image_pull_policy_and_crew_limits_to_docker_run() {
    use flotilla_protocol::ImageId;

    for (policy, docker_value) in
        [(ImagePullPolicy::Always, "always"), (ImagePullPolicy::IfNotPresent, "missing"), (ImagePullPolicy::Never, "never")]
    {
        let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let image = ImageId::new("flotilla-dev-env:latest");
        let opts = CreateOpts {
            tokens: Vec::new(),
            tools: vec![test_daemon_tool("/run/flotilla.sock")],
            working_directory: None,
            provisioned_mounts: Vec::new(),
            image_pull_policy: policy,
            prepared_auth: Default::default(),
            cpu_limit: None,
            memory_policy: Default::default(),
        };

        provider.create(EnvironmentId::new(docker_value), &image, opts).await.expect("image policy should create an environment");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        let (_, args, _) = &calls[0];
        assert!(args.windows(2).any(|pair| pair == ["--pull", docker_value]));
        // Crew vessels share CPU by equal weight and retain memory bounds.
        assert!(args.windows(2).any(|pair| pair == ["--cpu-shares", "1024"]));
        assert!(!args.iter().any(|arg| arg == "--cpus"));
        assert!(args.iter().any(|arg| arg == "--memory"));
        assert!(args.iter().any(|arg| arg == "--memory-swap"));
        assert!(!calls.iter().any(|(_, args, _)| args.first().is_some_and(|arg| arg == "pull")));
    }
}

#[tokio::test]
// Glue: Docker must lower the admitted registry artifact to its CLI configuration.
async fn create_uses_the_credential_scoped_docker_config_for_pull_on_run() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("registry.example/crew:latest");
    let opts = CreateOpts {
        tokens: Vec::new(),
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: Vec::new(),
        image_pull_policy: ImagePullPolicy::Always,
        prepared_auth: test_registry_auth("/run/flotilla/registry-auth"),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("private-registry"), &image, opts).await.expect("create authenticated environment");

    let calls = runner.calls();
    let (command, args, _) = &calls[0];
    assert_eq!(command, "docker");
    assert_eq!(&args[..3], &["--config", "/run/flotilla/registry-auth", "run"]);
    assert!(args.windows(2).any(|pair| pair == ["--pull", "always"]));
}

#[tokio::test]
async fn create_reports_infrastructure_and_requested_mount_metadata() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let reference_repo = DaemonHostPath::new("/host/reference-repo");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/host/daemon/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new(reference_repo.as_path().to_path_buf(), "/ref/repo", ProvisionedMountMode::Ro)],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let id = EnvironmentId::new("test-env-metadata");
    let handle = provider.create(id, &image, opts).await.expect("create");

    let calls = runner.calls();
    let (_, args, _) = &calls[0];
    assert!(
        args.windows(2).any(|pair| pair == ["-v", "/host/daemon/flotilla.sock:/run/flotilla-daemon/flotilla.sock:rw"]),
        "only the socket itself may be mounted; args: {args:?}",
    );
    assert_eq!(
        handle.provisioned_mounts(),
        vec![
            ProvisionedMount::new("/host/daemon/flotilla.sock", "/run/flotilla-daemon/flotilla.sock", ProvisionedMountMode::Rw),
            ProvisionedMount::new(reference_repo.as_path().to_path_buf(), "/ref/repo", ProvisionedMountMode::Ro),
        ],
        "docker provisioned environments should report infrastructure and requested bind mount metadata",
    );
}

#[tokio::test]
async fn create_rejects_a_mount_targeting_the_reserved_daemon_socket_directory() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/host/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new(
            "/host/replacement-socket-directory",
            "/run/flotilla-daemon",
            ProvisionedMountMode::Rw,
        )],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let error = provider
        .create(EnvironmentId::new("test-env-reserved-socket"), &image, opts)
        .await
        .err()
        .expect("reserved socket mount should be rejected");

    assert_eq!(error, "mount target /run/flotilla-daemon is reserved for the daemon socket");
    assert!(runner.calls().is_empty(), "reserved mount collisions should fail before invoking docker");
}

#[tokio::test]
async fn create_rejects_a_mount_targeting_a_reserved_tool_file() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let tool = EnvironmentTool::new("flotilla", "/usr/local/bin/flotilla").with_asset(EnvironmentToolAsset::new(
        "/host/flotilla",
        "/usr/local/bin/flotilla",
        EnvironmentToolAssetKind::File,
        EnvironmentToolAssetAccess::ReadOnly,
        "the flotilla CLI",
    ));
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![tool],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new("/host/replacement-flotilla", "/usr/local/bin/flotilla", ProvisionedMountMode::Ro)],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let error = provider
        .create(EnvironmentId::new("test-env-reserved-cli"), &image, opts)
        .await
        .err()
        .expect("reserved CLI mount should be rejected");

    assert_eq!(error, "mount target /usr/local/bin/flotilla is reserved for the flotilla CLI");
    assert!(runner.calls().is_empty(), "reserved mount collisions should fail before invoking docker");
}

#[tokio::test]
async fn create_delivers_tool_assets_and_applies_tool_environment() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let tool = EnvironmentTool::new("terminal", "/usr/local/bin/terminal")
        .with_asset(EnvironmentToolAsset::new(
            "/host/bin/terminal",
            "/usr/local/bin/terminal",
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "the terminal executable",
        ))
        .with_asset(EnvironmentToolAsset::new(
            "/host/state/terminal",
            "/var/lib/terminal",
            EnvironmentToolAssetKind::Directory,
            EnvironmentToolAssetAccess::SharedWritable,
            "terminal state",
        ))
        .with_environment(EnvironmentVariableUpdate::set("TERMINAL_STATE", "/var/lib/terminal", "terminal state"))
        .with_environment(EnvironmentVariableUpdate::prepend_path("LD_LIBRARY_PATH", "/usr/local/lib/terminal"));
    let opts = CreateOpts {
        tokens: vec![("LD_LIBRARY_PATH".to_string(), "/image/lib".to_string())],
        working_directory: None,
        provisioned_mounts: Vec::new(),
        tools: vec![tool],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("tool-delivery"), &image, opts).await.expect("Docker should lower provider-neutral tool assets");

    let args = &runner.calls()[0].1;
    assert!(args.contains(&"/host/bin/terminal:/usr/local/bin/terminal:ro".to_string()));
    assert!(args.contains(&"/host/state/terminal:/var/lib/terminal:rw".to_string()));
    assert!(args.contains(&"TERMINAL_STATE=/var/lib/terminal".to_string()));
    assert!(args.contains(&"LD_LIBRARY_PATH=/usr/local/lib/terminal:/image/lib".to_string()));
}

fn path_prepending_tool() -> EnvironmentTool {
    EnvironmentTool::new("cargo-shim", "/opt/flotilla/cargo-shim/cargo")
        .with_environment(EnvironmentVariableUpdate::prepend_path("PATH", "/opt/flotilla/cargo-shim"))
}

fn path_prepending_opts(image_pull_policy: ImagePullPolicy) -> CreateOpts {
    CreateOpts {
        tokens: Vec::new(),
        working_directory: None,
        provisioned_mounts: Vec::new(),
        tools: vec![path_prepending_tool()],
        image_pull_policy,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    }
}

fn docker_run_args(runner: &RecordingRunner) -> Vec<String> {
    runner
        .calls()
        .into_iter()
        .map(|(_, args, _)| args)
        .find(|args| args.first().map(String::as_str) == Some("run"))
        .expect("docker run call")
}

#[tokio::test]
async fn create_prepends_to_the_image_path_when_no_value_is_supplied() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(
        RecordingRunner::new_ok("container-id-123")
            .with_image_environment(Ok(r#"["PATH=/usr/local/cargo/bin:/usr/bin:/bin","RUSTUP_HOME=/usr/local/rustup"]"#)),
    );
    let provider = DockerEnvironmentProvider::new(runner.clone());

    provider
        .create(EnvironmentId::new("image-path"), &ImageId::new("crew:latest"), path_prepending_opts(ImagePullPolicy::IfNotPresent))
        .await
        .expect("create environment");

    let args = docker_run_args(&runner);
    assert!(args.contains(&"PATH=/opt/flotilla/cargo-shim:/usr/local/cargo/bin:/usr/bin:/bin".to_string()), "{args:?}");
    let pull = args.iter().position(|arg| arg == "--pull").expect("--pull flag");
    assert_eq!(args[pull + 1], "never", "the inspected image is the one that must run: {args:?}");
}

#[tokio::test]
async fn create_prepends_to_the_docker_default_path_when_the_image_sets_none() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123").with_image_environment(Ok("null")));
    let provider = DockerEnvironmentProvider::new(runner.clone());

    provider
        .create(EnvironmentId::new("default-path"), &ImageId::new("scratch-ish"), path_prepending_opts(ImagePullPolicy::IfNotPresent))
        .await
        .expect("create environment");

    let args = docker_run_args(&runner);
    assert!(
        args.contains(&"PATH=/opt/flotilla/cargo-shim:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()),
        "{args:?}"
    );
}

#[tokio::test]
async fn create_pulls_before_inspecting_when_the_policy_is_always() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123").with_image_environment(Ok(r#"["PATH=/usr/bin"]"#)));
    let provider = DockerEnvironmentProvider::new(runner.clone());

    provider
        .create(EnvironmentId::new("always-pull"), &ImageId::new("crew:latest"), path_prepending_opts(ImagePullPolicy::Always))
        .await
        .expect("create environment");

    let calls: Vec<Vec<String>> = runner.calls().into_iter().map(|(_, args, _)| args).collect();
    assert_eq!(calls[0], vec!["pull".to_string(), "--platform".to_string(), "linux/amd64".to_string(), "crew:latest".to_string()]);
    assert_eq!(calls[1][..4], ["image", "inspect", "--format", "{{json .Config.Env}}"]);
    assert!(docker_run_args(&runner).contains(&"PATH=/opt/flotilla/cargo-shim:/usr/bin".to_string()));
}

#[tokio::test]
async fn create_refuses_when_the_image_environment_cannot_be_inspected() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123").with_image_environment(Err("No such image: crew:latest")));
    let provider = DockerEnvironmentProvider::new(runner.clone());

    let error = provider
        .create(EnvironmentId::new("missing-image"), &ImageId::new("crew:latest"), path_prepending_opts(ImagePullPolicy::Never))
        .await
        .err()
        .expect("an uninspectable image should refuse provisioning");

    assert!(error.contains("crew:latest is not available locally"), "{error}");
    assert!(runner.calls().iter().all(|(_, args, _)| args.first().map(String::as_str) != Some("run")), "no container may start");
}

#[tokio::test]
async fn create_pulls_a_missing_image_before_reading_its_environment_when_the_policy_is_if_not_present() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(
        RecordingRunner::new_ok("container-id-123")
            .with_image_environment(Err("No such image: crew:latest"))
            .with_image_environment(Ok(r#"["PATH=/usr/local/cargo/bin:/usr/bin"]"#)),
    );
    let provider = DockerEnvironmentProvider::new(runner.clone());

    provider
        .create(
            EnvironmentId::new("missing-then-pulled"),
            &ImageId::new("crew:latest"),
            path_prepending_opts(ImagePullPolicy::IfNotPresent),
        )
        .await
        .expect("create environment");

    let calls: Vec<Vec<String>> = runner.calls().into_iter().map(|(_, args, _)| args).collect();
    assert_eq!(calls[0][..4], ["image", "inspect", "--format", "{{json .Config.Env}}"]);
    assert_eq!(calls[1], vec!["pull".to_string(), "--platform".to_string(), "linux/amd64".to_string(), "crew:latest".to_string()]);
    assert_eq!(calls[2][..4], ["image", "inspect", "--format", "{{json .Config.Env}}"]);
    let args = docker_run_args(&runner);
    assert!(args.contains(&"PATH=/opt/flotilla/cargo-shim:/usr/local/cargo/bin:/usr/bin".to_string()), "{args:?}");
    let pull = args.iter().position(|arg| arg == "--pull").expect("--pull flag");
    assert_eq!(args[pull + 1], "never", "{args:?}");
}

#[tokio::test]
async fn create_reports_both_inspect_and_pull_failures_when_a_missing_image_cannot_be_pulled() {
    use flotilla_protocol::ImageId;

    let runner =
        Arc::new(RecordingRunner::new_err("pull access denied for crew").with_image_environment(Err("No such image: crew:latest")));
    let provider = DockerEnvironmentProvider::new(runner.clone());

    let error = provider
        .create(EnvironmentId::new("unpullable"), &ImageId::new("crew:latest"), path_prepending_opts(ImagePullPolicy::IfNotPresent))
        .await
        .err()
        .expect("an unpullable missing image should refuse provisioning");

    assert!(error.contains("No such image: crew:latest"), "{error}");
    assert!(error.contains("pull access denied for crew"), "{error}");
    assert!(runner.calls().iter().all(|(_, args, _)| args.first().map(String::as_str) != Some("run")), "no container may start");
}

#[tokio::test]
async fn create_uses_the_bare_value_for_a_non_path_variable_the_image_does_not_set() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123").with_image_environment(Ok(r#"["PATH=/usr/bin"]"#)));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let mut opts = path_prepending_opts(ImagePullPolicy::IfNotPresent);
    opts.tools = vec![EnvironmentTool::new("terminal", "/usr/local/bin/terminal")
        .with_environment(EnvironmentVariableUpdate::prepend_path("LD_LIBRARY_PATH", "/usr/local/lib/terminal"))];

    provider.create(EnvironmentId::new("bare-value"), &ImageId::new("crew:latest"), opts).await.expect("create environment");

    let args = docker_run_args(&runner);
    assert!(args.contains(&"LD_LIBRARY_PATH=/usr/local/lib/terminal".to_string()), "{args:?}");
}

#[tokio::test]
async fn create_mounts_the_flotilla_binary_directory_so_atomic_replacements_stay_visible() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let tool = EnvironmentTool::new("flotilla", "/usr/local/bin/flotilla")
        .with_asset(EnvironmentToolAsset::new(
            "/host/flotilla/bin",
            "/opt/flotilla/bin",
            EnvironmentToolAssetKind::Directory,
            EnvironmentToolAssetAccess::ReadOnly,
            "the flotilla CLI",
        ))
        .with_asset(EnvironmentToolAsset::new(
            "/host/flotilla/launcher",
            "/usr/local/bin/flotilla",
            EnvironmentToolAssetKind::File,
            EnvironmentToolAssetAccess::ReadOnly,
            "the flotilla CLI launcher",
        ));
    let opts = CreateOpts {
        tokens: Vec::new(),
        working_directory: None,
        provisioned_mounts: Vec::new(),
        tools: vec![tool],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("upgrade-visible"), &image, opts).await.expect("create environment");

    let args = &runner.calls()[0].1;
    assert!(args.contains(&"/host/flotilla/bin:/opt/flotilla/bin:ro".to_string()), "binary parent must be the bind source: {args:?}");
    assert!(
        !args.contains(&"/host/flotilla/bin/flotilla:/opt/flotilla/bin/flotilla:ro".to_string()),
        "binary inode must not be pinned: {args:?}"
    );
}

// Glue contract: requested parent/child access modes survive Docker serialization.
// Generate zero through three own admins, including the empty and multi-checkout cases.
#[hegel::test]
fn create_uses_requested_mount_modes_in_docker_arguments(tc: hegel::TestCase) {
    let own_count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
    // Shared metadata can have a nonstandard name (for example separate-git-dir).
    let common = if tc.draw(hegel::generators::booleans()) { "/host/clone/.git" } else { "/host/metadata" };
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        use flotilla_protocol::ImageId;

        let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let image = ImageId::new("ubuntu:22.04");
        let mut opts = CreateOpts {
            tokens: vec![],
            tools: vec![test_daemon_tool("/run/flotilla.sock")],
            working_directory: None,
            provisioned_mounts: vec![
                ProvisionedMount::new("/host/workspace", "/workspace", ProvisionedMountMode::Rw),
                ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro),
                ProvisionedMount::new(common, common, ProvisionedMountMode::Rw),
                ProvisionedMount::new(format!("{common}/config"), format!("{common}/config"), ProvisionedMountMode::Ro),
                ProvisionedMount::new(format!("{common}/hooks"), format!("{common}/hooks"), ProvisionedMountMode::Ro),
                ProvisionedMount::new(format!("{common}/worktrees"), format!("{common}/worktrees"), ProvisionedMountMode::Ro),
            ],
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            prepared_auth: Default::default(),
            cpu_limit: None,
            memory_policy: Default::default(),
        };

        for own in 0..own_count {
            let path = format!("{common}/worktrees/own-{own}");
            opts.provisioned_mounts.push(ProvisionedMount::new(&path, &path, ProvisionedMountMode::Rw));
        }

        provider.create(EnvironmentId::new("test-env-mount-modes"), &image, opts).await.expect("create");

        let calls = runner.calls();
        let (_, args, _) = &calls[0];
        assert!(
            args.windows(2).any(|pair| pair == ["-v", "/host/workspace:/workspace:rw"]),
            "writable workspace mount should be passed to docker as :rw; args: {args:?}",
        );
        assert!(
            args.windows(2).any(|pair| pair == ["-v", "/host/reference-repo:/ref/repo:ro"]),
            "read-only reference mount should be passed to docker as :ro; args: {args:?}",
        );
        // #2682: the parent protects all siblings, including registrations created later.
        for protected in ["config", "hooks", "worktrees"] {
            let expected = format!("type=bind,source={common}/{protected},target={common}/{protected},readonly");
            assert!(
                args.windows(2).any(|pair| pair == ["--mount", expected.as_str()]),
                "protected Git metadata must be a strict read-only bind: {args:?}"
            );
        }
        for own in 0..own_count {
            let expected = format!("{common}/worktrees/own-{own}:{common}/worktrees/own-{own}:rw");
            assert!(args.windows(2).any(|pair| pair == ["-v", expected.as_str()]), "own admin stays writable: {args:?}");
        }
    });
}

#[tokio::test]
async fn protected_git_mount_rejects_a_comma_in_its_path() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new("/host/a,b/.git/config", "/host/a,b/.git/config", ProvisionedMountMode::Ro)],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };
    let error = provider.create(EnvironmentId::new("comma"), &ImageId::new("ubuntu:22.04"), opts).await.err().expect("unsafe mount syntax");
    assert!(error.contains("comma"), "{error}");
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn list_preserves_provisioned_mount_metadata() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id-123".into()),
        Ok("sha256:test-image".into()),
        Ok(format!(
            "container-1\ttest-env-list\tubuntu:22.04\t{}\n",
            serde_json::to_string(&vec![
                ProvisionedMount::new("/run", "/run/flotilla-daemon", ProvisionedMountMode::Rw),
                ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro),
            ])
            .expect("serialize mount metadata")
        )),
    ]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro)],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("test-env-list"), &image, opts).await.expect("create");
    let handles = provider.list().await.expect("list");

    assert_eq!(handles.len(), 1);
    assert_eq!(
        handles[0].provisioned_mounts(),
        vec![
            ProvisionedMount::new("/run", "/run/flotilla-daemon", ProvisionedMountMode::Rw),
            ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro),
        ],
        "docker list should preserve flotilla-managed bind mount metadata",
    );
}

#[tokio::test]
async fn list_defaults_mount_mode_from_pre_mode_metadata() {
    let runner = Arc::new(RecordingRunner::new_ok(
        "container-1\ttest-env-list\tubuntu:22.04\t[{\"host_path\":\"/host/reference-repo\",\"environment_path\":\"/ref/repo\"}]\n",
    ));
    let provider = DockerEnvironmentProvider::new(runner);

    let handles = provider.list().await.expect("pre-mode mount metadata should remain readable");

    assert_eq!(
        handles[0].provisioned_mounts(),
        vec![ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro)],
        "mounts written before mode existed were mounted read-only",
    );
}

#[tokio::test]
async fn list_fails_on_malformed_reference_repo_mount_metadata() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id-123".into()),
        Ok("sha256:test-image".into()),
        Ok("container-1\ttest-env-list\tubuntu:22.04\tnot-json\n".into()),
    ]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro)],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("test-env-list-malformed"), &image, opts).await.expect("create");
    let result = provider.list().await;

    assert!(result.is_err(), "malformed mount metadata must fail listing");
}

#[tokio::test]
async fn list_rejects_missing_reference_repo_mount_metadata() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id-123".into()),
        Ok("sha256:test-image".into()),
        Ok("container-1\ttest-env-list\tubuntu:22.04\t\n".into()),
    ]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro)],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    provider.create(EnvironmentId::new("test-env-list-missing"), &image, opts).await.expect("create");
    let result = provider.list().await;

    assert!(result.is_err(), "missing mount metadata must fail listing");
}

#[tokio::test]
async fn provisioned_handle_returns_its_initialized_runner() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner);
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let handle = provider.create(EnvironmentId::new("test-env-runner"), &image, opts).await.expect("create");
    let first_runner = handle.runner();
    let second_runner = handle.runner();

    assert!(Arc::ptr_eq(&first_runner, &second_runner), "runner should be initialized once on the handle");
}

#[tokio::test]
async fn status_returns_running() {
    use flotilla_protocol::ImageId;
    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id".into()), // docker run
        Ok("sha256:test-image".into()),
        Ok(include_str!("fixtures/normal-stop.json").replace("exited", "running")), // full inspect
    ]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let id = EnvironmentId::new("test-env-status");
    let handle = provider.create(id, &image, opts).await.expect("create");
    let status = handle.observe().await.state;

    assert_eq!(status, super::ObservationState::Running);
    let calls = runner.calls();
    // Third call should inspect container status after image digest resolution.
    let (cmd, args, _) = &calls[2];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "inspect");
    assert_eq!(args, &["inspect", "container-id"]);
}

// #2790: read the container configuration, without login-shell mutations or
// truncating empty, multiline, or equals-containing values. Docker's
// runtime HOME fills an absent declaration, but never replaces explicit HOME.
// Named examples suffice for this Docker query/JSON decoding glue.
#[tokio::test]
async fn env_vars_reads_configured_container_environment() {
    use flotilla_protocol::ImageId;
    // Cover absent, explicit, and explicitly empty HOME, plus null Config.Env.
    for (configured_home, null_environment) in [(None, false), (Some("/configured-home"), false), (Some(""), false), (None, true)] {
        let mut entries = vec!["FOO=bar", "BAZ=qux", "LIBGL_ALWAYS_SOFTWARE=1", "TEXT=line one\nline two=tail", "EMPTY=", "MALFORMED"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if let Some(home) = configured_home {
            entries.push(format!("HOME={home}"));
        }
        let configured = if null_environment { "null".to_string() } else { serde_json::to_string(&entries).expect("JSON") };
        let mut responses = vec![Ok("container-id".into()), Ok("sha256:test-image".into()), Ok(configured)];
        if configured_home.is_none() {
            responses.push(Ok("/home/crew".into()));
        }
        let runner = Arc::new(QueuedRunner::new(responses));
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let image = ImageId::new("ubuntu:22.04");
        let opts = CreateOpts {
            tokens: vec![],
            tools: vec![test_daemon_tool("/run/flotilla.sock")],
            working_directory: None,
            provisioned_mounts: vec![],
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            prepared_auth: Default::default(),
            cpu_limit: None,
            memory_policy: Default::default(),
        };

        let id = EnvironmentId::new("test-env-vars");
        let handle = provider.create(id, &image, opts).await.expect("create");
        let vars = handle.env_vars().await.expect("env_vars");

        if null_environment {
            // A null configuration contributes no variables; only the vessel's
            // resolved HOME is added, never host or previously queried values.
            assert_eq!(vars, std::collections::HashMap::from([("HOME".to_string(), "/home/crew".to_string())]));
        } else {
            assert_eq!(vars.get("FOO"), Some(&"bar".to_string()));
            assert_eq!(vars.get("BAZ"), Some(&"qux".to_string()));
            assert_eq!(vars.get("LIBGL_ALWAYS_SOFTWARE").map(String::as_str), Some("1"));
            assert_eq!(vars.get("TEXT").map(String::as_str), Some("line one\nline two=tail"));
            assert_eq!(vars.get("EMPTY").map(String::as_str), Some(""));
        }
        assert!(!vars.contains_key("MALFORMED"));
        assert_eq!(vars.get("HOME").map(String::as_str), Some(configured_home.unwrap_or("/home/crew")));

        let calls = runner.calls();
        let (cmd, args, _) = &calls[2];
        assert_eq!(cmd, "docker");
        assert_eq!(args, &["inspect", "--format", "{{json .Config.Env}}", "flotilla-env-test-env-vars"]);
        assert_eq!(calls.len(), if configured_home.is_none() { 4 } else { 3 });
        if configured_home.is_none() {
            assert_eq!(calls[3].1, ["exec", "flotilla-env-test-env-vars", "sh", "-c", "printf %s \"${HOME-}\""]);
        }
    }
}

#[tokio::test]
async fn destroy_calls_docker_rm() {
    use flotilla_protocol::ImageId;
    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id".into()), // docker run
        Ok("sha256:test-image".into()),
        Ok("".into()), // docker rm -f
    ]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        prepared_auth: Default::default(),
        cpu_limit: None,
        memory_policy: Default::default(),
    };

    let id = EnvironmentId::new("test-env-destroy");
    let handle = provider.create(id, &image, opts).await.expect("create");
    let backing_id = handle.backing_id().to_string();
    handle.destroy().await.expect("destroy");

    let calls = runner.calls();
    let (cmd, args, _) = &calls[2];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "rm");
    assert!(args.contains(&"-f".to_string()), "should pass -f flag");
    assert_eq!(backing_id, "container-id");
    assert!(args.contains(&backing_id), "destroy must use the immutable backing ID");
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

/// Verifies that DockerEnvironmentRunner composes correctly with the CleatTerminalPoolFactory:
/// the factory's binary probe arrives via docker exec, demonstrating the decorator
/// pattern works end-to-end with real factory logic.
#[tokio::test]
async fn environment_runner_supports_factory_probe() {
    use crate::test_config::TestConfig;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_terminal::factories::cleat::CleatTerminalPoolFactory;

    // A runner that succeeds for any docker exec call (simulates cleat present in container)
    let inner = Arc::new(RecordingRunner::new_ok("cleat 0.5.0"));
    let env_runner = Arc::new(DockerEnvironmentRunner::new("test-container".to_string(), inner.clone()));

    // Build an EnvironmentBag that asserts cleat is available at the path the factory expects
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"));

    let dir = tempfile::tempdir().expect("tempdir");
    let config = TestConfig::with_base(dir.path());
    let repo_root = ExecutionEnvironmentPath::new("/repo");

    // The factory checks env.find_binary("cleat") first — it does NOT call runner for binary detection.
    // Passing the DockerEnvironmentRunner as the runner proves the decorator is accepted by the factory
    // and that CleatTerminalPool is constructed with it, proving the composition path.
    let result = CleatTerminalPoolFactory.probe(&bag, &config, &repo_root, env_runner.clone()).await;
    assert!(result.is_ok(), "probe should succeed when cleat binary assertion is present");

    // Verify that no actual docker exec calls were made during probe (factory only checks bag)
    let calls = inner.calls();
    assert!(calls.is_empty(), "factory probe should not invoke runner during binary check");
}

#[tokio::test]
async fn docker_cleat_launch_does_not_forward_outer_terminal_identity() {
    use crate::test_config::TestConfig;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_replay_testkit::testing::MockRunner;
    use flotilla_terminal::factories::cleat::CleatTerminalPoolFactory;

    let inner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("--env-clear --env".into()), Ok("{}".into())]));
    let env_runner = Arc::new(DockerEnvironmentRunner::new("test-container".to_string(), inner.clone()));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"))
        .with(EnvironmentAssertion::env_var("TERM", "screen-256color"))
        .with(EnvironmentAssertion::env_var("TERM_PROGRAM", "outer-terminal"))
        .with(EnvironmentAssertion::env_var("TERM_PROGRAM_VERSION", "1.2.3"))
        .with(EnvironmentAssertion::env_var("COLORTERM", "truecolor"));
    let dir = tempfile::tempdir().expect("tempdir");
    let config = TestConfig::with_base(dir.path());
    let repo_root = ExecutionEnvironmentPath::new("/repo");

    let pool = CleatTerminalPoolFactory.probe(&bag, &config, &repo_root, env_runner).await.expect("cleat pool in Docker");
    pool.ensure_session("session", "codex", &repo_root, &vec![], &[]).await.expect("launch in Docker");

    assert_eq!(
        inner.calls()[2].1,
        vec![
            "exec",
            "-w",
            "/",
            "test-container",
            "/usr/bin/env",
            "-i",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "/usr/local/bin/cleat",
            "launch",
            "--env-clear",
            "--json",
            "--record",
            "session",
            "--cwd",
            "/repo",
            "--cmd",
            "codex",
            "--env",
            "PATH=/usr/local/bin:/usr/bin:/bin",
        ]
    );
}

/// Verifies that DockerEnvironmentRunner correctly transforms command calls into docker exec form,
/// matching the pattern that discovery factories would issue inside a container.
#[tokio::test]
async fn environment_runner_transforms_commands_for_container() {
    // Simulate the exact check a discovery factory might perform: "cleat --version"
    let inner = Arc::new(RecordingRunner::new_ok("cleat 0.5.0"));
    let env_runner = DockerEnvironmentRunner::new("my-container".to_string(), inner.clone());
    let label = ChannelLabel::Default;

    // This is the kind of command a binary-check probe would issue
    env_runner.run("cleat", &["--version"], Path::new("/"), &label).await.ok();

    let calls = inner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, cwd) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args, &["exec", "-w", "/", "my-container", "cleat", "--version"]);
    assert_eq!(cwd, Path::new("/"));
}

// #2653: every contained environment must bound RAM and disable swap by default.
#[tokio::test]
async fn create_bounds_memory_and_swap() {
    let runner = Arc::new(RecordingRunner::new_ok("container-id"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    provider
        .create(
            EnvironmentId::new("memory-test"),
            &flotilla_protocol::ImageId::new("test-image"),
            CreateOpts {
                tokens: Vec::new(),
                working_directory: None,
                provisioned_mounts: Vec::new(),
                tools: Vec::new(),
                image_pull_policy: ImagePullPolicy::Never,
                prepared_auth: Default::default(),
                cpu_limit: None,
                memory_policy: Default::default(),
            },
        )
        .await
        .expect("create");
    let calls = runner.calls();
    let args = &calls.iter().find(|(cmd, args, _)| cmd == "docker" && args.first().is_some_and(|arg| arg == "run")).expect("docker run").1;
    assert!(args.windows(2).any(|pair| pair == ["--memory", "13743895347"]));
    assert!(args.windows(2).any(|pair| pair == ["--memory-swap", "13743895347"]));
}

// Subprocess boundary: replay the operator's real inspect/journal recordings.
struct InspectionRunner {
    inspect: String,
    usage: Option<String>,
    oomd: Option<String>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl CommandRunner for InspectionRunner {
    async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
        match cmd {
            "docker" if args.first() == Some(&"ps") => Ok("container\tenv-test\timage\t[]".into()),
            "docker" if args == ["inspect", "container"] => Ok(self.inspect.clone()),
            "journalctl" if args.contains(&"--unit=systemd-oomd") => self.oomd.clone().ok_or("journal access denied".into()),
            "journalctl" => Err("kernel journal access denied".into()),
            "cat" => self.usage.clone().ok_or("cgroup unavailable".into()),
            _ => Err(format!("unexpected command: {cmd} {args:?}")),
        }
    }
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        true
    }
}

// #2653: true cgroup OOM, host oomd, ordinary failure and normal SIGTERM
// are distinct, and recorded limits survive classification.
#[tokio::test]
async fn recorded_container_deaths_are_classified_through_the_provider() {
    use flotilla_protocol::EnvironmentExitCause;
    let cases = [
        (include_str!("fixtures/cgroup-oomkilled.json"), None, EnvironmentExitCause::CgroupOom, 137, Some(9), true),
        (
            include_str!("fixtures/oomd-exit137.json"),
            Some(include_str!("fixtures/oomd-journal.txt").into()),
            EnvironmentExitCause::HostOomd,
            137,
            Some(9),
            false,
        ),
        (include_str!("fixtures/nonzero-exit3.json"), None, EnvironmentExitCause::NonzeroExit, 3, None, false),
        (include_str!("fixtures/normal-stop.json"), None, EnvironmentExitCause::NormalStop, 143, Some(15), false),
        (include_str!("fixtures/oomd-exit137.json"), None, EnvironmentExitCause::Signal, 137, Some(9), false),
    ];
    use super::{GoneCause, ObservationState};
    let expected = [
        GoneCause::MemoryLimitReached,
        GoneCause::HostOutOfMemory,
        GoneCause::BackingExited { code: 3 },
        GoneCause::GuestPoweredOff,
        GoneCause::BackingExited { code: 137 },
    ];
    for ((inspect, oomd, cause, code, signal, oom_killed), expected) in cases.into_iter().zip(expected) {
        let runner = Arc::new(InspectionRunner { inspect: inspect.into(), usage: None, oomd, calls: Mutex::new(Vec::new()) });
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let handles = provider.list().await.expect("list");
        let observed = handles[0].observe().await;
        assert_eq!(observed.state, ObservationState::Gone { cause: expected });
        let observation = observed.runtime.expect("Docker observation");
        let termination = observation.termination.expect("terminated");
        assert_eq!(
            (termination.cause, termination.exit_code, termination.signal, termination.oom_killed),
            (cause, code, signal, oom_killed)
        );
        if cause == EnvironmentExitCause::CgroupOom {
            assert_eq!(
                observation.memory_limits,
                Some(flotilla_protocol::EnvironmentMemoryLimits { memory_bytes: 67108864, swap_bytes: 0 })
            );
        }
        if cause == EnvironmentExitCause::HostOomd {
            assert!(termination
                .evidence
                .expect("matching journal evidence")
                .contains("dbb8a5519c4d1f0529c9ac380b9e61b51b75275359dc2e282a61370deab3a12d.scope for killing"));
        }
        let calls = runner.calls.lock().expect("calls");
        for (_, args) in calls.iter().filter(|(cmd, _)| cmd == "journalctl") {
            assert!(args.contains(&"--since".into()) && args.contains(&"--until".into()), "journal evidence must be time bounded");
        }
    }
}

// #2653: a candidate-list mention does not prove oomd killed this container.
// The generator spans absent evidence, candidates only, wrong scope and a kill.
#[hegel::test]
fn oomd_requires_a_matching_kill_record(tc: hegel::TestCase) {
    use flotilla_protocol::EnvironmentExitCause;
    use hegel::generators as gs;
    let choice = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let journal = include_str!("fixtures/oomd-journal.txt");
    let evidence = match choice {
        0 => String::new(),
        1 => journal.lines().filter(|line| !line.contains("Marked ")).collect::<Vec<_>>().join("\n"),
        2 => journal.replace("dbb8a5519c4d1f0529c9ac380b9e61b51b75275359dc2e282a61370deab3a12d", "other-container"),
        _ => journal.into(),
    };
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let provider = DockerEnvironmentProvider::new(Arc::new(InspectionRunner {
            inspect: include_str!("fixtures/oomd-exit137.json").into(),
            usage: None,
            oomd: Some(evidence),
            calls: Mutex::new(Vec::new()),
        }));
        let handles = provider.list().await.expect("list");
        let termination = handles[0].observe().await.runtime.expect("Docker observation").termination.expect("termination");
        assert_eq!(termination.cause, if choice == 3 { EnvironmentExitCause::HostOomd } else { EnvironmentExitCause::Signal });
    });
}

// #2653: placement policy changes the hard RAM and combined RAM/swap flags.
#[tokio::test]
async fn create_uses_configured_memory_policy() {
    let runner = Arc::new(RecordingRunner::new_ok("container-id"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let mut opts = path_prepending_opts(ImagePullPolicy::Never);
    opts.tools.clear();
    opts.memory_policy =
        flotilla_resources::EnvironmentMemoryPolicy { host_memory_percent: 25, expected_concurrent_crews: 2, swap_bytes: 1073741824 };
    provider.create(EnvironmentId::new("configured-memory"), &flotilla_protocol::ImageId::new("image"), opts).await.expect("create");
    let args = docker_run_args(&runner);
    assert!(args.windows(2).any(|pair| pair == ["--memory", "8589934592"]));
    assert!(args.windows(2).any(|pair| pair == ["--memory-swap", "9663676416"]));
    assert!(!args.iter().any(|arg| arg == "--cgroup-parent"));
    assert!(!runner.calls().iter().any(|(cmd, _, _)| cmd == "systemctl"));
}

// With [docker_vessels], a crew starts under the slice with memory = memory-swap
// = the ceiling whatever the policy's share, and only once the slice is loaded
// under Docker's systemd driver with a finite MemoryMax no lower than the
// ceiling, on a host with at least the ceiling in RAM. Otherwise provisioning
// refuses, naming the slice, before any container runs. Each case draws the
// ceiling and the policy, then tries host RAM one GiB below, at and above the
// ceiling against every systemctl answer (MemoryMax one GiB below, at and above the ceiling, in either
// property order; a missing slice; `infinity`, u64::MAX and absent MemoryMax;
// an unreadable unit) under both cgroup drivers.
#[hegel::test]
fn docker_vessels_set_parent_and_ceiling_or_refuse(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let gib = tc.draw(gs::integers::<u64>().min_value(2).max_value(256));
    let policy = flotilla_resources::EnvironmentMemoryPolicy {
        host_memory_percent: tc.draw(gs::integers::<u8>().min_value(1).max_value(100)),
        expected_concurrent_crews: tc.draw(gs::integers::<u32>().min_value(1).max_value(20)),
        swap_bytes: tc.draw(gs::integers::<u64>().min_value(0).max_value(1 << 30)),
    };
    let slice = "flotilla-crews.slice";
    let mut shows: Vec<(Result<String, String>, bool)> = Vec::new();
    for max_gib in [gib - 1, gib, gib + 1] {
        let max = max_gib << 30;
        shows.push((Ok(format!("MemoryMax={max}\nLoadState=loaded\n")), max_gib >= gib));
        shows.push((Ok(format!("LoadState=loaded\nMemoryMax={max}\n")), max_gib >= gib));
    }
    shows.extend([
        (Ok("LoadState=not-found\nMemoryMax=infinity\n".into()), false),
        (Ok("LoadState=loaded\nMemoryMax=infinity\n".into()), false),
        (Ok(format!("LoadState=loaded\nMemoryMax={}\n", u64::MAX)), false),
        (Ok("LoadState=loaded\n".into()), false),
        (Err("Failed to connect to bus".into()), false),
    ]);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    for ((show, show_ok), driver, ram_gib) in shows.iter().flat_map(|show| {
        ["systemd", "cgroupfs"].into_iter().flat_map(move |driver| [gib - 1, gib, gib + 1].map(|ram_gib| (show.clone(), driver, ram_gib)))
    }) {
        let mut runner = RecordingRunner::new_ok("container-id");
        let ram = ram_gib << 30;
        runner.memory_info = Some(Ok(format!(r#"{{"MemTotal":{ram},"MemoryLimit":true,"SwapLimit":true,"CgroupDriver":"{driver}"}}"#)));
        runner.slice_show = show.clone();
        let runner = Arc::new(runner);
        let vessels = toml::from_str(&format!("slice = {slice:?}\nmemory_ceiling_gib = {gib}\n")).expect("table");
        let vessels = super::docker_vessels::DockerVessels::parse(vessels).expect("valid [docker_vessels]");
        let provider = DockerEnvironmentProvider::new(runner.clone()).with_vessel_slice(Some(vessels));
        let mut opts = path_prepending_opts(ImagePullPolicy::Never);
        opts.tools.clear();
        opts.memory_policy = policy.clone();
        let created = runtime.block_on(provider.create(EnvironmentId::new("sliced"), &flotilla_protocol::ImageId::new("image"), opts));
        if show_ok && driver == "systemd" && ram_gib >= gib {
            created.unwrap_or_else(|error| panic!("{show:?} should provision: {error}"));
            let args = docker_run_args(&runner);
            let ceiling = (gib << 30).to_string();
            assert!(args.windows(2).any(|pair| pair == ["--cgroup-parent", slice]), "{args:?}");
            assert!(args.windows(2).any(|pair| pair == ["--memory", ceiling.as_str()]), "{args:?}");
            assert!(args.windows(2).any(|pair| pair == ["--memory-swap", ceiling.as_str()]), "{args:?}");
        } else {
            let error = created.err().unwrap_or_else(|| panic!("{show:?} under {driver} with {ram_gib} GiB should refuse"));
            assert!(error.contains(slice), "{error}");
            assert!(!runner.calls().iter().any(|(cmd, args, _)| cmd == "docker" && args.first().is_some_and(|arg| arg == "run")));
        }
    }
}

// #2653: unavailable capacity or unsupported enforcement must never run an
// unbounded crew. Invalid/malformed/empty outputs and unsupported kernels refuse.
#[tokio::test]
async fn create_refuses_unenforceable_memory_limits() {
    for info in [
        Err("Docker info unavailable".into()),
        Ok(String::new()),
        Ok(r#"{"MemTotal":0,"MemoryLimit":true,"SwapLimit":true}"#.into()),
        Ok(r#"{"MemTotal":68719476736,"MemoryLimit":false,"SwapLimit":true}"#.into()),
        Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":false}"#.into()),
    ] {
        let mut runner = RecordingRunner::new_ok("container-id");
        runner.memory_info = Some(info);
        let runner = Arc::new(runner);
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let mut opts = path_prepending_opts(ImagePullPolicy::Never);
        opts.tools.clear();
        assert!(provider.create(EnvironmentId::new("unsupported-memory"), &flotilla_protocol::ImageId::new("image"), opts).await.is_err());
        assert!(!runner.calls().iter().any(|(_, args, _)| args.first().is_some_and(|arg| arg == "run")));
    }
}

// Last-known RAM usage comes from the cgroup counter under the container's own
// slice. Each case draws a counter response (zero, large, unavailable/empty,
// malformed or overflowing) and reads it under every parent: default,
// top-level, nested and non-slice.
#[hegel::test]
fn running_memory_samples_do_not_invent_usage(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let choice = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let bytes = tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX));
    let (usage, counter) = match choice {
        0 => (Some(format!("{bytes}\n")), Some(bytes)),
        1 => (None, None),
        2 => (Some(String::new()), None),
        3 => (Some("not a counter".into()), None),
        _ => (Some("18446744073709551616".into()), None),
    };
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    for (parent, slice) in [
        ("", Some("system.slice")),
        ("crews.slice", Some("crews.slice")),
        ("flotilla-crews.slice", Some("flotilla.slice/flotilla-crews.slice")),
        ("a-b-c.slice", Some("a.slice/a-b.slice/a-b-c.slice")),
        ("/docker", None),
    ] {
        let expected = counter.filter(|_| slice.is_some());
        // A synthetic running state derived from the real inspect shape; death
        // recordings remain verbatim. The subprocess counter is the varied boundary.
        let mut inspect: serde_json::Value = serde_json::from_str(include_str!("fixtures/cgroup-oomkilled.json")).expect("fixture");
        inspect[0]["State"]["Status"] = "running".into();
        inspect[0]["HostConfig"]["CgroupParent"] = parent.into();
        let id = inspect[0]["Id"].as_str().expect("container id").to_string();
        let runner =
            Arc::new(InspectionRunner { inspect: inspect.to_string(), usage: usage.clone(), oomd: None, calls: Mutex::new(Vec::new()) });
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let observation = runtime.block_on(async {
            let handles = provider.list().await.expect("list");
            handles[0].observe().await.runtime.expect("Docker observation")
        });
        let read: Vec<_> =
            runner.calls.lock().expect("calls").iter().filter(|(cmd, _)| cmd == "cat").map(|(_, args)| args.clone()).collect();
        let path = slice.map(|slice| vec![format!("/sys/fs/cgroup/{slice}/docker-{id}.scope/memory.current")]);
        assert_eq!(read, path.into_iter().collect::<Vec<_>>(), "{parent}");
        assert_eq!(observation.memory_usage_bytes, expected);
        assert_eq!(observation.memory_observed_at.is_some(), expected.is_some());
        assert!(observation.termination.is_none());
    }
}

// #2840 recovery needs only a full immutable container ID and the environment
// label; mount/image labels from earlier generations must not block listing.
#[tokio::test]
async fn list_backings_uses_immutable_identity_without_mount_metadata() {
    let id = "a".repeat(64);
    let runner = Arc::new(RecordingRunner::new_ok(&format!("{id}\tenv-orphan\n")));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let backings = provider.list_backings().await.expect("list backing identities");
    assert_eq!(backings, vec![super::EnvironmentBacking { environment_id: EnvironmentId::new("env-orphan"), backing_id: id }]);
    assert_eq!(
        runner.calls(),
        vec![(
            "docker".into(),
            vec![
                "ps".into(),
                "-a".into(),
                "--no-trunc".into(),
                "--filter".into(),
                "label=flotilla.environment".into(),
                "--format".into(),
                r#"{{.ID}}\t{{.Label "flotilla.environment"}}"#.into(),
            ],
            PathBuf::from("/")
        )]
    );
}

// Empty listings are valid. Incomplete IDs or labels must fail closed, rather
// than yielding a partially trustworthy set that could authorize reclamation.
#[tokio::test]
async fn list_backings_rejects_incomplete_identity() {
    for output in
        ["short\tenv\n".into(), format!("{}\t\n", "a".repeat(64)), "missing separator".into(), format!("{}\tenv\textra\n", "a".repeat(64))]
    {
        let provider = DockerEnvironmentProvider::new(Arc::new(RecordingRunner::new_ok(&output)));
        assert!(provider.list_backings().await.is_err(), "invalid identity {output:?}");
    }
    let provider = DockerEnvironmentProvider::new(Arc::new(RecordingRunner::new_ok("")));
    assert!(provider.list_backings().await.expect("empty listing").is_empty());
}

// Docker provisioning consumes only its own preparation, uses the prepared
// image without an implicit pull, and exposes the environment runner and mounts.
#[tokio::test]
#[cfg(unix)]
async fn docker_provisions_prepared_digest_and_refuses_another_instance() {
    let runner = Arc::new(QueuedRunner::new([
        Ok("held".into()),
        Ok("container-id".into()),
        Ok(format!("sha256:{}", "a".repeat(64))),
        Ok(r#"["HOME=/tmp","PATH=/usr/bin:/bin"]"#.into()),
    ]));
    let provider = super::contract_tests::docker_provider(runner.clone());
    let other = super::contract_tests::docker_provider(runner.clone());
    let prepared = provider.prepare(&pinned_spec(), &Default::default()).await.expect("prepare held digest");
    let id = EnvironmentId::new("prepared-docker");
    assert!(other.provision(id.clone(), &prepared, Default::default()).await.is_err());
    let handle = provider.provision(id.clone(), &prepared, Default::default()).await.expect("provision");
    assert_eq!(handle.id(), &id);
    assert_eq!(handle.image().as_str(), pinned_spec().docker.expect("docker").image);
    assert_eq!(handle.provisioned_mounts().len(), 1);
    assert_eq!(handle.provisioned_mounts()[0].environment_path.as_path(), Path::new("/run/drydock"));
    let calls = runner.calls();
    let run = calls.iter().find(|(_, args, _)| args.first().is_some_and(|arg| arg == "run")).expect("container run");
    assert!(run.1.windows(2).any(|args| args == ["--pull", "never"]));
}

// Missing pinned registry digests can be pulled with admitted credentials;
// preparation reports pull/verification failures without classifying them as build demand.
// The queued runner stands in for Docker's subprocess interface.
#[tokio::test]
#[cfg(unix)]
async fn preparation_pulls_only_pinned_registry_content() {
    let mut spec = pinned_spec();
    spec.docker.as_mut().expect("docker").image = format!("registry.example/crew@sha256:{}", "a".repeat(64));
    let mut opts = super::PrepareOpts { prepared_auth: test_registry_auth("/private/operation-auth"), ..Default::default() };
    let runner = Arc::new(QueuedRunner::new([Err("not held".into()), Ok("pulled".into()), Ok("held".into())]));
    let provider = super::contract_tests::docker_provider(runner.clone());
    provider.prepare(&spec, &opts).await.expect("pull existing digest");
    let calls = runner.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[1].1,
        ["--config", "/private/operation-auth", "pull", "--platform", "linux/amd64", spec.docker.as_ref().expect("docker").image.as_str()]
    );
    let absent = super::contract_tests::docker_provider(Arc::new(QueuedRunner::new([Err("not held".into()), Err("not published".into())])));
    opts.prepared_auth = test_registry_auth("/private/operation-auth");
    let error = absent.prepare(&spec, &opts).await.err().expect("pull failed");
    assert!(error.starts_with("pinned image pull failed:"));
    let absent_after_pull = super::contract_tests::docker_provider(Arc::new(QueuedRunner::new([
        Err("not held".into()),
        Ok("pulled".into()),
        Err("still absent".into()),
    ])));
    opts.prepared_auth = test_registry_auth("/private/operation-auth");
    assert_eq!(
        absent_after_pull.prepare(&spec, &opts).await.err().expect("verify failed"),
        "pinned image pull succeeded but digest is not present"
    );
}

// Tags and malformed digests cannot make preparation succeed, even if a
// subprocess would claim they are present. No subprocess runs for invalid pins.
#[tokio::test]
async fn preparation_refuses_mutable_or_malformed_image_identity() {
    for image in ["crew:latest", "registry.example/crew@sha256:invalid", "sha256:invalid", ""] {
        let runner = Arc::new(RecordingRunner::new_ok("held"));
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let mut spec = pinned_spec();
        spec.docker.as_mut().expect("docker").image = image.into();
        assert!(provider.prepare(&spec, &Default::default()).await.is_err());
        assert!(runner.calls().is_empty());
    }
}

// Host-side Docker inspection seam: distinguish weight from old quotas and
// accept inspections that predate explicit CPU settings.
#[tokio::test]
async fn inspection_reports_weight_and_legacy_quotas() {
    for (cpu, cpus, shares) in [
        (serde_json::json!({"CpuShares": 1024, "NanoCpus": 0}), None, Some(1024)),
        (serde_json::json!({"NanoCpus": 8_000_000_000_u64}), Some(8), None),
        (serde_json::json!({"CpuQuota": 400000, "CpuPeriod": 100000, "CpuShares": 1024}), Some(4), Some(1024)),
        (serde_json::json!({"CpuQuota": -1, "CpuPeriod": 100000}), None, None),
        (serde_json::json!({}), None, None),
    ] {
        // Construct inspection responses at the Docker subprocess boundary.
        let mut inspect: serde_json::Value = serde_json::from_str(include_str!("fixtures/normal-stop.json")).expect("fixture");
        for (key, value) in cpu.as_object().expect("CPU fields") {
            inspect[0]["HostConfig"][key] = value.clone();
        }
        let runner = Arc::new(InspectionRunner { inspect: inspect.to_string(), usage: None, oomd: None, calls: Mutex::new(Vec::new()) });
        let provider = DockerEnvironmentProvider::new(runner);
        let handles = provider.list().await.expect("list");
        let observed = handles[0].observe().await.runtime.expect("Docker observation");
        let limits = observed.configured_limits.expect("inspected CPU settings");
        assert_eq!((limits.cpus, limits.cpu_shares), (cpus, shares));
    }
}

// Reserved tool directories reject ancestor and descendant mount overlaps;
// adjacent names sharing a text prefix remain independent filesystem paths.
#[tokio::test]
async fn create_rejects_nested_tool_mounts_but_accepts_adjacent_paths() {
    for (target, rejected) in [("/reserved", true), ("/reserved/child", true), ("/", true), ("/reserved-other", false)] {
        let runner = Arc::new(RecordingRunner::new_ok("container"));
        let provider = DockerEnvironmentProvider::new(runner);
        let opts = CreateOpts {
            tools: vec![EnvironmentTool::new("tool", "/bin/tool").with_asset(EnvironmentToolAsset::new(
                "/host/tool",
                "/reserved",
                EnvironmentToolAssetKind::Directory,
                EnvironmentToolAssetAccess::ReadOnly,
                "the tool",
            ))],
            provisioned_mounts: vec![ProvisionedMount::new("/host/request", target, ProvisionedMountMode::Ro)],
            ..Default::default()
        };
        let result = provider.create(EnvironmentId::new("nested"), &flotilla_protocol::ImageId::new("image"), opts).await;
        assert_eq!(result.is_err(), rejected, "target {target}");
    }
}

// Unsupported platforms still exercise the provider's explicit refusal rather
// than compiling a Unix transport fixture into the Windows test binary.
#[cfg(not(unix))]
#[tokio::test]
async fn docker_agent_provisioning_reports_unsupported_host() {
    let provider = DockerEnvironmentProvider::new(Arc::new(RecordingRunner::new_ok("")));
    let prepared = super::PreparedEnvironment::new(&Arc::new(()), ());
    let error = provider.provision(EnvironmentId::new("unsupported"), &prepared, Default::default()).await.err().unwrap();
    assert!(matches!(error, super::EnvironmentProvisionError::Failed(message) if message.contains("require a Unix host")));
}

// Unsupported guest targets refuse before any image preparation or guest push.
#[cfg(unix)]
#[tokio::test]
async fn docker_refuses_unsupported_requested_guest_before_execution() {
    for platform in [drydock_protocol::Platform::new("linux", "arm64"), drydock_protocol::Platform::new("darwin", "arm64")] {
        let runner = Arc::new(RecordingRunner::new_ok("held"));
        let provider = super::contract_tests::docker_provider(runner.clone());
        let result = provider.prepare(&pinned_spec(), &super::PrepareOpts { platform: Some(platform.clone()), ..Default::default() }).await;
        assert!(result.err().expect("unsupported target").contains(&format!("guest platform {platform}")));
        assert!(runner.calls().is_empty());
    }
}

#[tokio::test]
#[cfg(unix)]
async fn a_mutable_tag_is_refused_before_any_docker_call() {
    let runner = Arc::new(RecordingRunner::new_ok("present"));
    let provider = super::contract_tests::docker_provider(runner.clone());
    let mut spec = pinned_spec();
    let docker = spec.docker.as_mut().expect("docker");
    docker.image = "registry.example/crew:latest".into();
    docker.image_build_ref = None;
    let error = provider.prepare(&spec, &Default::default()).await.err().expect("mutable tag");
    assert!(error.contains("mutable tags are unsupported"), "{error}");
    assert!(runner.calls().is_empty());
}
