use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, EnvironmentStatus, HostName, ImageSource};

use super::{
    contained_daemon_socket_path, docker::DockerEnvironmentProvider, runner::DockerEnvironmentRunner, CreateOpts, EnvironmentProvider,
    EnvironmentTool, EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind, EnvironmentVariableUpdate,
    ImagePullPolicy, ProvisionedMount, ProvisionedMountMode,
};
use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};

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
}

impl RecordingRunner {
    fn new_ok(output: &str) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            result: Ok(output.to_string()),
            image_environment: Mutex::new(VecDeque::new()),
            memory_info: None,
        }
    }

    fn new_err(msg: &str) -> Self {
        Self { calls: Mutex::new(vec![]), result: Err(msg.to_string()), image_environment: Mutex::new(VecDeque::new()), memory_info: None }
    }

    /// Answers the next `docker image inspect --format '{{json .Config.Env}}'`
    /// with `output`. Repeated calls queue successive answers; the last one
    /// answers every later inspection.
    fn with_image_environment(self, output: Result<&str, &str>) -> Self {
        self.image_environment.lock().expect("image environment mutex").push_back(output.map(str::to_string).map_err(str::to_string));
        self
    }

    fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
        self.calls.lock().expect("calls mutex").clone()
    }
}

#[async_trait]
impl CommandRunner for RecordingRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        // Host capacity probe fixture, independent of the command under test.
        if cmd == "docker" && args == ["info", "--format", "{{json .}}"] {
            return self
                .memory_info
                .clone()
                .unwrap_or_else(|| Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into()));
        }
        self.calls.lock().expect("calls mutex").push((cmd.to_string(), args.iter().map(|a| a.to_string()).collect(), cwd.to_path_buf()));
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
}

impl QueuedRunner {
    fn new(responses: impl IntoIterator<Item = Result<String, String>>) -> Self {
        Self { calls: Mutex::new(vec![]), responses: Mutex::new(responses.into_iter().collect()) }
    }

    fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
        self.calls.lock().expect("calls mutex").clone()
    }
}

#[async_trait]
impl CommandRunner for QueuedRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        // Host capacity probe fixture, independent of the command under test.
        if cmd == "docker" && args == ["info", "--format", "{{json .}}"] {
            return Ok(r#"{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}"#.into());
        }
        self.calls.lock().expect("calls mutex").push((cmd.to_string(), args.iter().map(|a| a.to_string()).collect(), cwd.to_path_buf()));
        let mut queue = self.responses.lock().expect("responses mutex");
        queue.pop_front().unwrap_or(Err("no more responses".into()))
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
    use super::EnvironmentKind;
    use hegel::generators as gs;
    let host = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let docker = tc.draw(gs::integers::<u8>().min_value(0).max_value(1)) == 1;
    let mut spec = pinned_spec();
    if !docker {
        spec.docker = None;
    }
    spec.host_direct =
        host.then(|| flotilla_resources::HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/".into() });
    assert_eq!(EnvironmentKind::of(&spec).is_ok(), host != docker);
}

fn pinned_spec() -> flotilla_resources::EnvironmentSpec {
    super::legacy_environment_spec(&EnvironmentSpec {
        image: ImageSource::Registry(format!("sha256:{}", "a".repeat(64))),
        token_env_vars: Vec::new(),
    })
    .expect("registry spec")
}

// The subprocess double represents the Docker CLI. A held digest prepares
// without building or pulling; absent local-only digests wait on ImageBuild.
#[tokio::test]
async fn preparation_uses_existing_digest_and_never_builds() {
    for present in [true, false] {
        let runner = Arc::new(RecordingRunner::new_ok("present"));
        let missing = Arc::new(RecordingRunner::new_err("absent"));
        let selected = if present { runner } else { missing };
        let provider = DockerEnvironmentProvider::new(selected.clone());
        assert_eq!(provider.prepare(&pinned_spec(), &Default::default()).await.is_ok(), present);
        assert_eq!(selected.calls().len(), 1);
        assert_eq!(selected.calls()[0].1[0..2], ["image", "inspect"]);
    }
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
    assert_eq!(calls[2].1, ["rm", "-f", "flotilla-env-invalid-digest"]);
}

#[tokio::test]
async fn create_translates_image_pull_policy_to_docker_run() {
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
            cpu_limit: Some(8),
            memory_policy: Default::default(),
        };

        provider.create(EnvironmentId::new(docker_value), &image, opts).await.expect("image policy should create an environment");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        let (_, args, _) = &calls[0];
        assert!(args.windows(2).any(|pair| pair == ["--pull", docker_value]));
        assert!(args.windows(2).any(|pair| pair == ["--cpus", "8"]));
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
        prepared_auth: super::PreparedEnvironmentAuth::RegistryConfig { directory: DaemonHostPath::new("/run/flotilla/registry-auth") },
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
        args.windows(2).any(|pair| pair == ["-v", "/host/daemon:/run/flotilla-daemon:rw"]),
        "daemon socket parent directory should be mounted read-write; args: {args:?}",
    );
    assert_eq!(
        handle.provisioned_mounts(),
        vec![
            ProvisionedMount::new("/host/daemon", "/run/flotilla-daemon", ProvisionedMountMode::Rw),
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
    assert_eq!(calls[0], vec!["pull".to_string(), "crew:latest".to_string()]);
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
    assert_eq!(calls[1], vec!["pull".to_string(), "crew:latest".to_string()]);
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
        Ok("running".into()), // docker inspect status
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
    let status = handle.status().await.expect("status");

    assert_eq!(status, EnvironmentStatus::Running);
    let calls = runner.calls();
    // Third call should inspect container status after image digest resolution.
    let (cmd, args, _) = &calls[2];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "inspect");
    assert!(args.contains(&"--format".to_string()));
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
    let container_name = format!("flotilla-env-{}", handle.id());
    handle.destroy().await.expect("destroy");

    let calls = runner.calls();
    let (cmd, args, _) = &calls[2];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "rm");
    assert!(args.contains(&"-f".to_string()), "should pass -f flag");
    assert!(args.contains(&container_name), "should pass container name");
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

/// Verifies that DockerEnvironmentRunner composes correctly with the CleatTerminalPoolFactory:
/// the factory's binary probe arrives via docker exec, demonstrating the decorator
/// pattern works end-to-end with real factory logic.
#[tokio::test]
async fn environment_runner_supports_factory_probe() {
    use crate::{
        config::ConfigStore,
        providers::discovery::{factories::cleat::CleatTerminalPoolFactory, EnvironmentAssertion, EnvironmentBag, Factory},
    };
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    // A runner that succeeds for any docker exec call (simulates cleat present in container)
    let inner = Arc::new(RecordingRunner::new_ok("cleat 0.5.0"));
    let env_runner = Arc::new(DockerEnvironmentRunner::new("test-container".to_string(), inner.clone()));

    // Build an EnvironmentBag that asserts cleat is available at the path the factory expects
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"));

    let dir = tempfile::tempdir().expect("tempdir");
    let config = ConfigStore::with_base(dir.path());
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
    use crate::config::ConfigStore;
    use crate::providers::discovery::factories::cleat::CleatTerminalPoolFactory;
    use crate::providers::discovery::EnvironmentAssertion;
    use crate::providers::discovery::EnvironmentBag;
    use crate::providers::discovery::Factory;
    use crate::testkits::replay::testing::MockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    let inner = Arc::new(MockRunner::new(vec![Ok("[]".into()), Ok("--env-clear --env".into()), Ok("{}".into())]));
    let env_runner = Arc::new(DockerEnvironmentRunner::new("test-container".to_string(), inner.clone()));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"))
        .with(EnvironmentAssertion::env_var("TERM", "screen-256color"))
        .with(EnvironmentAssertion::env_var("TERM_PROGRAM", "outer-terminal"))
        .with(EnvironmentAssertion::env_var("TERM_PROGRAM_VERSION", "1.2.3"))
        .with(EnvironmentAssertion::env_var("COLORTERM", "truecolor"));
    let dir = tempfile::tempdir().expect("tempdir");
    let config = ConfigStore::with_base(dir.path());
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

/// Integration test: three-hop composition — SSH → docker exec → terminal attach.
///
/// Builds a HopPlan with RemoteToHost + EnterEnvironment + RunCommand and resolves
/// it end-to-end using mock resolvers. Asserts that the output is correctly nested:
/// SSH wrapping docker exec wrapping the terminal attach command.
#[test]
fn hop_chain_resolves_remote_plus_environment_plus_terminal() {
    use std::collections::HashMap;

    use flotilla_protocol::arg::{flatten, Arg};

    use crate::hop_chain::{
        environment::DockerEnvironmentHopResolver, remote::RemoteHopResolver, resolver::HopResolver, Hop, HopPlan, ResolutionContext,
        ResolvedAction,
    };

    // ── Mock resolvers ───────────────────────────────────────────────

    /// A minimal mock RemoteHopResolver for wrap mode:
    /// pops the inner Command, wraps with ssh <host> <NestedCommand(inner)>.
    struct MockRemote;
    impl RemoteHopResolver for MockRemote {
        fn resolve_wrap(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String> {
            let inner_action = context.actions.pop().ok_or("mock: no inner action")?;
            let ResolvedAction::Command(inner_args) = inner_action;
            let mut ssh_args = vec![Arg::Literal("ssh".into()), Arg::Quoted(host.as_str().to_string())];
            ssh_args.push(Arg::NestedCommand(inner_args));
            context.actions.push(ResolvedAction::Command(ssh_args));
            Ok(())
        }
    }

    // ── Build the HopResolver ────────────────────────────────────────

    let mut containers = HashMap::new();
    containers.insert(EnvironmentId::new("env1"), "container-abc".to_string());
    let docker_env = Arc::new(DockerEnvironmentHopResolver::new(containers));

    let resolver = HopResolver::new(Arc::new(MockRemote), docker_env);

    // ── Build the HopPlan: RemoteToHost → EnterEnvironment → RunCommand ──

    let plan = HopPlan(vec![
        Hop::RemoteToHost { host: HostName::new("feta") },
        Hop::EnterEnvironment { env_id: EnvironmentId::new("env1"), provider: "docker".into() },
        Hop::RunCommand { command: vec![Arg::Literal("cleat".into()), Arg::Literal("attach".into()), Arg::Literal("sess-123".into())] },
    ]);

    // ── Resolve from a different host ────────────────────────────────

    let mut context = ResolutionContext {
        current_host: HostName::new("local-host"),
        current_environment: None,
        working_directory: None,
        actions: Vec::new(),
        nesting_depth: 0,
    };

    let resolved = resolver.resolve(&plan, &mut context).expect("resolve should succeed");

    // ── Assert output structure ──────────────────────────────────────

    // Should produce a single Command action (all wrapped)
    assert_eq!(resolved.0.len(), 1, "three-hop wrap should produce exactly one Command action");

    let ResolvedAction::Command(outer_args) = &resolved.0[0];

    // Outermost: ssh <host> <NestedCommand(...)>
    assert_eq!(outer_args[0], Arg::Literal("ssh".into()), "outermost command should be ssh");
    assert_eq!(outer_args[1], Arg::Quoted("feta".into()), "ssh target should be feta");
    assert_eq!(outer_args.len(), 3, "ssh args should have exactly 3 elements (ssh, target, nested)");

    // Middle: a client-side supervisor wrapping docker exec and the terminal attach.
    let docker_nested = match &outer_args[2] {
        Arg::NestedCommand(args) => args,
        other => panic!("expected NestedCommand for docker layer, got {other:?}"),
    };
    assert_eq!(docker_nested[0], Arg::Literal("sh".into()), "middle command should be the supervisor shell");
    assert_eq!(docker_nested[1], Arg::Literal("-c".into()));
    let Arg::Quoted(wrapper) = &docker_nested[2] else { panic!("expected quoted supervisor") };
    assert!(wrapper.contains("trap cleanup EXIT HUP INT TERM"));
    assert!(wrapper.contains("kill -KILL \"$pid\""));
    assert_eq!(docker_nested[4], Arg::Quoted("container-abc".into()), "docker exec target should be container-abc");

    // Innermost args are flattened directly into the docker exec invocation
    assert_eq!(docker_nested[7], Arg::Literal("cleat".into()), "innermost command should be cleat");
    assert_eq!(docker_nested[8], Arg::Literal("attach".into()), "cleat subcommand should be attach");
    assert_eq!(docker_nested[9], Arg::Literal("sess-123".to_string()), "cleat should attach to correct session");
    assert_eq!(docker_nested.len(), 10, "docker nested should have exactly 10 args");

    // Verify flatten produces the expected structure
    let flat = flatten(outer_args, 0);
    assert!(flat.starts_with("ssh "), "flattened output should start with ssh: {flat}");
    assert!(flat.contains("docker exec"), "should contain supervised docker exec: {flat}");
    assert!(flat.contains("container-abc"), "should contain the quoted container target: {flat}");
    assert!(flat.contains("cleat attach"), "should contain cleat attach: {flat}");
    assert!(flat.contains("sess-123"), "should contain session id: {flat}");

    // Verify nesting depth updated for both remote and environment hops
    assert_eq!(context.nesting_depth, 2, "nesting_depth should be 2 after remote + environment hops");
    assert_eq!(context.current_host.as_str(), "feta", "current_host should be updated to feta");
    assert_eq!(context.current_environment, Some(EnvironmentId::new("env1")), "current_environment should be env1");
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
    for (inspect, oomd, cause, code, signal, oom_killed) in cases {
        let runner = Arc::new(InspectionRunner { inspect: inspect.into(), usage: None, oomd, calls: Mutex::new(Vec::new()) });
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let handles = provider.list().await.expect("list");
        let observation = handles[0].runtime_observation().await.expect("inspect").expect("Docker observation");
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
        let termination = handles[0].runtime_observation().await.expect("inspect").expect("observation").termination.expect("termination");
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

// Last-known RAM usage comes from the cgroup counter. Generate zero, large
// counters, unavailable/empty, malformed and overflowing responses explicitly.
#[hegel::test]
fn running_memory_samples_do_not_invent_usage(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let choice = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let bytes = tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX));
    let (usage, expected) = match choice {
        0 => (Some(format!("{bytes}\n")), Some(bytes)),
        1 => (None, None),
        2 => (Some(String::new()), None),
        3 => (Some("not a counter".into()), None),
        _ => (Some("18446744073709551616".into()), None),
    };
    // A synthetic running state derived from the real inspect shape; death
    // recordings remain verbatim. The subprocess counter is the varied boundary.
    let mut inspect: serde_json::Value = serde_json::from_str(include_str!("fixtures/cgroup-oomkilled.json")).expect("fixture");
    inspect[0]["State"]["Status"] = "running".into();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let provider = DockerEnvironmentProvider::new(Arc::new(InspectionRunner {
            inspect: inspect.to_string(),
            usage,
            oomd: None,
            calls: Mutex::new(Vec::new()),
        }));
        let handles = provider.list().await.expect("list");
        let observation = handles[0].runtime_observation().await.expect("inspect").expect("observation");
        assert_eq!(observation.memory_usage_bytes, expected);
        assert_eq!(observation.memory_observed_at.is_some(), expected.is_some());
        assert!(observation.termination.is_none());
    });
}

// #2840 recovery needs only a full immutable container ID and the environment
// label; mount/image labels from earlier generations must not block listing.
#[tokio::test]
async fn list_backings_uses_immutable_identity_without_mount_metadata() {
    let id = "a".repeat(64);
    let runner = Arc::new(RecordingRunner::new_ok(&format!("{id}\tenv-orphan\n")));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let backings = provider.list_backings().await.expect("list backing identities");
    assert_eq!(backings, vec![super::EnvironmentBacking { environment_id: EnvironmentId::new("env-orphan"), container_id: id }]);
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

// Explicit instance selection never crosses kinds. Generate zero through three
// same-kind instances, including ambiguity, exact hits and an absent identity.
#[hegel::test]
fn provider_selection_is_kind_and_instance_scoped(tc: hegel::TestCase) {
    use super::{host_direct::HostDirectEnvironmentProvider, EnvironmentKind};
    use crate::providers::{
        discovery::{ProviderCategory, ProviderDescriptor},
        registry::ProviderRegistry,
    };
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let mut registry = ProviderRegistry::new();
    let runner = Arc::new(RecordingRunner::new_ok(""));
    registry.environment_providers.insert(
        "direct",
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "direct"),
        Arc::new(HostDirectEnvironmentProvider::new(runner.clone(), Default::default())),
    );
    for index in 0..count {
        let identity = format!("endpoint-{index}");
        registry.environment_providers.insert(
            &identity,
            ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, &identity),
            Arc::new(DockerEnvironmentProvider::new(runner.clone())),
        );
    }
    assert_eq!(registry.environment_providers.select(EnvironmentKind::Docker, None).is_some(), count == 1);
    assert_eq!(registry.environment_providers.select(EnvironmentKind::Docker, Some("endpoint-0")).is_some(), count > 0);
    assert!(registry.environment_providers.select(EnvironmentKind::Docker, Some("direct")).is_none());
    assert!(registry.environment_providers.select(EnvironmentKind::HostDirect, Some("absent")).is_none());
    assert!(registry.environment_providers.select(EnvironmentKind::HostDirect, None).is_some());
}

// Docker provisioning consumes only its own preparation, uses the prepared
// image without an implicit pull, and exposes the environment runner and mounts.
#[tokio::test]
async fn docker_provisions_prepared_digest_and_refuses_another_instance() {
    let runner = Arc::new(QueuedRunner::new([Ok("held".into()), Ok("container-id".into()), Ok(format!("sha256:{}", "a".repeat(64)))]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let other = DockerEnvironmentProvider::new(runner.clone());
    let prepared = provider.prepare(&pinned_spec(), &Default::default()).await.expect("prepare held digest");
    let id = EnvironmentId::new("prepared-docker");
    assert!(other.provision(id.clone(), &prepared, Default::default()).await.is_err());
    let handle = provider.provision(id.clone(), &prepared, Default::default()).await.expect("provision");
    assert_eq!(handle.id(), &id);
    assert_eq!(handle.image().as_str(), pinned_spec().docker.expect("docker").image);
    assert!(handle.provisioned_mounts().is_empty());
    let calls = runner.calls();
    let run = calls.iter().find(|(_, args, _)| args.first().is_some_and(|arg| arg == "run")).expect("container run");
    assert!(run.1.windows(2).any(|args| args == ["--pull", "never"]));
}

// Missing pinned registry digests can be pulled with admitted credentials;
// preparation reports pull/verification failures without classifying them as build demand.
// The queued runner stands in for Docker's subprocess interface.
#[tokio::test]
async fn preparation_pulls_only_pinned_registry_content() {
    let mut spec = pinned_spec();
    spec.docker.as_mut().expect("docker").image = format!("registry.example/crew@sha256:{}", "a".repeat(64));
    let opts = super::PrepareOpts {
        legacy_baseline: false,
        prepared_auth: super::PreparedEnvironmentAuth::RegistryConfig { directory: DaemonHostPath::new("/private/operation-auth") },
    };
    let runner = Arc::new(QueuedRunner::new([Err("not held".into()), Ok("pulled".into()), Ok("held".into())]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    provider.prepare(&spec, &opts).await.expect("pull existing digest");
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].1, ["--config", "/private/operation-auth", "pull", spec.docker.as_ref().expect("docker").image.as_str()]);
    let absent = DockerEnvironmentProvider::new(Arc::new(QueuedRunner::new([Err("not held".into()), Err("not published".into())])));
    let error = absent.prepare(&spec, &opts).await.err().expect("pull failed");
    assert!(error.starts_with("pinned image pull failed:"));
    let absent_after_pull = DockerEnvironmentProvider::new(Arc::new(QueuedRunner::new([
        Err("not held".into()),
        Ok("pulled".into()),
        Err("still absent".into()),
    ])));
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

// Live pre-#2731 fleet records carry a baseline tag and no ImageBuild ref.
// A controller-admitted baseline remains launchable, but runs the resolved digest.
#[tokio::test]
async fn live_baseline_tag_without_image_build_provisions_a_digest() {
    let tag = "forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-10-02.d9e59d8c.dbd6e440";
    let digest = format!("sha256:{}", "b".repeat(64));
    for held in [false, true] {
        let runner = Arc::new(QueuedRunner::new([
            if held { Ok("held".into()) } else { Err("not held".into()) },
            Ok("pulled".into()),
            Ok(digest.clone()),
            Ok("container-id".into()),
            Ok(digest.clone()),
        ]));
        let provider = DockerEnvironmentProvider::new(runner.clone());
        let mut spec = pinned_spec();
        let docker = spec.docker.as_mut().expect("docker");
        docker.image = tag.into();
        docker.image_build_ref = None;
        docker.image_composition = None;
        docker.pull_policy = flotilla_resources::DockerImagePullPolicy::Always;
        let prepared = provider
            .prepare(&spec, &super::PrepareOpts { legacy_baseline: true, ..Default::default() })
            .await
            .expect("prepare live baseline");
        let handle =
            provider.provision(EnvironmentId::new("live-baseline"), &prepared, Default::default()).await.expect("launch live record");
        assert_eq!(handle.image().as_str(), digest);
        let calls = runner.calls();
        assert_eq!(calls[1].1, ["pull", tag]);
        assert!(calls[3].1.contains(&digest));
        assert!(!calls[3].1.contains(&tag.to_string()));
        assert!(calls[3].1.windows(2).any(|args| args == ["--pull", "never"]));
    }
}
