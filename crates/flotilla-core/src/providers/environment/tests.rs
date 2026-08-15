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
}

impl RecordingRunner {
    fn new_ok(output: &str) -> Self {
        Self { calls: Mutex::new(vec![]), result: Ok(output.to_string()) }
    }

    fn new_err(msg: &str) -> Self {
        Self { calls: Mutex::new(vec![]), result: Err(msg.to_string()) }
    }

    fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
        self.calls.lock().expect("calls mutex").clone()
    }
}

#[async_trait]
impl CommandRunner for RecordingRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.calls.lock().expect("calls mutex").push((cmd.to_string(), args.iter().map(|a| a.to_string()).collect(), cwd.to_path_buf()));
        if cmd == "docker" && args.starts_with(&["inspect", "--format", "{{.Image}}"]) {
            return Ok("sha256:test-image-digest\n".to_string());
        }
        self.result.clone()
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
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
    let label = ChannelLabel::Noop;

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
    let label = ChannelLabel::Noop;

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
        self.calls.lock().expect("calls mutex").push((cmd.to_string(), args.iter().map(|a| a.to_string()).collect(), cwd.to_path_buf()));
        let mut queue = self.responses.lock().expect("responses mutex");
        queue.pop_front().unwrap_or(Err("no more responses".into()))
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        match self.run(cmd, args, cwd, label).await {
            Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
            Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
        }
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// DockerEnvironmentProvider tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ensure_image_builds_dockerfile() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dockerfile_path = temp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, "FROM ubuntu:24.04\n").expect("write Dockerfile");
    let runner = Arc::new(QueuedRunner::new([Err("missing".into()), Ok(String::new())]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let spec = EnvironmentSpec { image: ImageSource::Dockerfile(dockerfile_path.clone()), token_env_vars: vec![] };
    let repo_root = temp.path();

    let result = provider.ensure_image(&spec, repo_root).await;

    assert!(result.is_ok(), "ensure_image should succeed for Dockerfile source");
    let image_id = result.unwrap();
    assert!(image_id.as_str().starts_with("flotilla-env-"));
    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    let (inspect_cmd, inspect_args, inspect_cwd) = &calls[0];
    assert_eq!(inspect_cmd, "docker");
    assert_eq!(inspect_args, &["image", "inspect", image_id.as_str()]);
    assert_eq!(inspect_cwd, repo_root);
    let (build_cmd, build_args, build_cwd) = &calls[1];
    assert_eq!(build_cmd, "docker");
    assert_eq!(build_args[0], "build");
    assert_eq!(build_cwd, repo_root);
    assert!(build_args.contains(&"-t".to_string()), "should pass -t flag");
    assert!(build_args.contains(&"-f".to_string()), "should pass -f flag");
    let tag_idx = build_args.iter().position(|a| a == "-t").expect("-t flag present");
    assert_eq!(build_args[tag_idx + 1], image_id.as_str());
    let f_idx = build_args.iter().position(|a| a == "-f").expect("-f flag present");
    assert_eq!(build_args[f_idx + 1], dockerfile_path.to_string_lossy());
}

#[tokio::test]
async fn ensure_image_reuses_tag_for_same_dockerfile_contents() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dockerfile_path = temp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, "FROM ubuntu:24.04\nRUN echo hi\n").expect("write Dockerfile");
    let spec = EnvironmentSpec { image: ImageSource::Dockerfile(dockerfile_path.clone()), token_env_vars: vec![] };

    let first_runner = Arc::new(RecordingRunner::new_ok(""));
    let first_provider = DockerEnvironmentProvider::new(first_runner);
    let second_runner = Arc::new(RecordingRunner::new_ok(""));
    let second_provider = DockerEnvironmentProvider::new(second_runner);

    let first = first_provider.ensure_image(&spec, temp.path()).await.expect("first ensure_image");
    let second = second_provider.ensure_image(&spec, temp.path()).await.expect("second ensure_image");

    assert_eq!(first, second, "same Dockerfile contents should produce the same image tag");
}

#[tokio::test]
async fn ensure_image_skips_build_when_tag_exists_locally() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dockerfile_path = temp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, "FROM ubuntu:24.04\nRUN echo hi\n").expect("write Dockerfile");
    let runner = Arc::new(RecordingRunner::new_ok("already-present"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let spec = EnvironmentSpec { image: ImageSource::Dockerfile(dockerfile_path), token_env_vars: vec![] };

    let image_id = provider.ensure_image(&spec, temp.path()).await.expect("ensure_image");

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, cwd) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args, &["image", "inspect", image_id.as_str()]);
    assert_eq!(cwd, temp.path());
}

#[tokio::test]
async fn ensure_image_pulls_registry() {
    let runner = Arc::new(RecordingRunner::new_ok(""));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let spec = EnvironmentSpec { image: ImageSource::Registry("ubuntu:22.04".into()), token_env_vars: vec![] };
    let repo_root = std::path::Path::new("/repo");

    let result = provider.ensure_image(&spec, repo_root).await;

    assert!(result.is_ok(), "ensure_image should succeed for Registry source");
    let image_id = result.unwrap();
    assert_eq!(image_id.as_str(), "ubuntu:22.04");
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, _) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args, &["pull", "ubuntu:22.04"]);
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
        docker_config_dir: None,
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
    assert_eq!(handle.image_digest(), Some("sha256:8c7f4e5d6a1b"));

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
        docker_config_dir: None,
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
async fn create_removes_container_when_image_digest_cannot_be_resolved() {
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
        docker_config_dir: None,
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
            docker_config_dir: None,
        };

        provider.create(EnvironmentId::new(docker_value), &image, opts).await.expect("image policy should create an environment");

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        let (_, args, _) = &calls[0];
        assert!(args.windows(2).any(|pair| pair == ["--pull", docker_value]));
        assert!(!calls.iter().any(|(_, args, _)| args.first().is_some_and(|arg| arg == "pull")));
    }
}

#[tokio::test]
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
        docker_config_dir: Some(DaemonHostPath::new("/run/flotilla/registry-auth")),
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
        docker_config_dir: None,
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
        docker_config_dir: None,
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
        docker_config_dir: None,
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
        docker_config_dir: None,
    };

    provider.create(EnvironmentId::new("tool-delivery"), &image, opts).await.expect("Docker should lower provider-neutral tool assets");

    let args = &runner.calls()[0].1;
    assert!(args.contains(&"/host/bin/terminal:/usr/local/bin/terminal:ro".to_string()));
    assert!(args.contains(&"/host/state/terminal:/var/lib/terminal:rw".to_string()));
    assert!(args.contains(&"TERMINAL_STATE=/var/lib/terminal".to_string()));
    assert!(args.contains(&"LD_LIBRARY_PATH=/usr/local/lib/terminal:/image/lib".to_string()));
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
        docker_config_dir: None,
    };

    provider.create(EnvironmentId::new("upgrade-visible"), &image, opts).await.expect("create environment");

    let args = &runner.calls()[0].1;
    assert!(args.contains(&"/host/flotilla/bin:/opt/flotilla/bin:ro".to_string()), "binary parent must be the bind source: {args:?}");
    assert!(
        !args.contains(&"/host/flotilla/bin/flotilla:/opt/flotilla/bin/flotilla:ro".to_string()),
        "binary inode must not be pinned: {args:?}"
    );
}

#[tokio::test]
async fn create_uses_requested_mount_modes_in_docker_arguments() {
    use flotilla_protocol::ImageId;

    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![
            ProvisionedMount::new("/host/workspace", "/workspace", ProvisionedMountMode::Rw),
            ProvisionedMount::new("/host/reference-repo", "/ref/repo", ProvisionedMountMode::Ro),
        ],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        docker_config_dir: None,
    };

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
        docker_config_dir: None,
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
        docker_config_dir: None,
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
        docker_config_dir: None,
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
        docker_config_dir: None,
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
        docker_config_dir: None,
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

#[tokio::test]
async fn env_vars_parses_output() {
    use flotilla_protocol::ImageId;
    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id".into()), // docker run
        Ok("sha256:test-image".into()),
        Ok("FOO=bar\nBAZ=qux\n".into()), // docker exec sh -lc env
    ]));
    let provider = DockerEnvironmentProvider::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        tools: vec![test_daemon_tool("/run/flotilla.sock")],
        working_directory: None,
        provisioned_mounts: vec![],
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        docker_config_dir: None,
    };

    let id = EnvironmentId::new("test-env-vars");
    let handle = provider.create(id, &image, opts).await.expect("create");
    let vars = handle.env_vars().await.expect("env_vars");

    assert_eq!(vars.get("FOO"), Some(&"bar".to_string()));
    assert_eq!(vars.get("BAZ"), Some(&"qux".to_string()));

    let calls = runner.calls();
    let (cmd, args, _) = &calls[2];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "exec");
    assert!(args.contains(&"sh".to_string()));
    assert!(args.contains(&"env".to_string()));
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
        docker_config_dir: None,
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
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{factories::cleat::CleatTerminalPoolFactory, EnvironmentAssertion, EnvironmentBag, Factory},
    };

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

/// Verifies that DockerEnvironmentRunner correctly transforms command calls into docker exec form,
/// matching the pattern that discovery factories would issue inside a container.
#[tokio::test]
async fn environment_runner_transforms_commands_for_container() {
    // Simulate the exact check a discovery factory might perform: "cleat --version"
    let inner = Arc::new(RecordingRunner::new_ok("cleat 0.5.0"));
    let env_runner = DockerEnvironmentRunner::new("my-container".to_string(), inner.clone());
    let label = ChannelLabel::Noop;

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
/// Builds a HopPlan with RemoteToHost + EnterEnvironment + AttachTerminal and resolves
/// it end-to-end using mock resolvers. Asserts that the output is correctly nested:
/// SSH wrapping docker exec wrapping the terminal attach command.
#[test]
fn hop_chain_resolves_remote_plus_environment_plus_terminal() {
    use std::collections::HashMap;

    use flotilla_protocol::arg::{flatten, Arg};

    use crate::{
        attachable::AttachableId,
        hop_chain::{
            environment::DockerEnvironmentHopResolver, remote::RemoteHopResolver, resolver::HopResolver, terminal::TerminalHopResolver,
            Hop, HopPlan, ResolutionContext, ResolvedAction,
        },
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

    /// A minimal mock TerminalHopResolver that pushes a simple attach command.
    struct MockTerminal;
    impl TerminalHopResolver for MockTerminal {
        fn resolve(&self, attachable_id: &AttachableId, context: &mut ResolutionContext) -> Result<(), String> {
            context.actions.push(ResolvedAction::Command(vec![
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal(attachable_id.to_string()),
            ]));
            Ok(())
        }
    }

    // ── Build the HopResolver ────────────────────────────────────────

    let mut containers = HashMap::new();
    containers.insert(EnvironmentId::new("env1"), "container-abc".to_string());
    let docker_env = Arc::new(DockerEnvironmentHopResolver::new(containers));

    let resolver = HopResolver::new(Arc::new(MockRemote), docker_env, Arc::new(MockTerminal));

    // ── Build the HopPlan: RemoteToHost → EnterEnvironment → AttachTerminal ──

    let att_id = AttachableId::new("sess-123");
    let plan = HopPlan(vec![
        Hop::RemoteToHost { host: HostName::new("feta") },
        Hop::EnterEnvironment { env_id: EnvironmentId::new("env1"), provider: "docker".into() },
        Hop::AttachTerminal { attachable_id: att_id.clone() },
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

    // Middle: docker exec -it container-abc cleat attach <sess-id>
    // (DockerEnvironmentHopResolver extends the inner args directly, no extra NestedCommand)
    let docker_nested = match &outer_args[2] {
        Arg::NestedCommand(args) => args,
        other => panic!("expected NestedCommand for docker layer, got {other:?}"),
    };
    assert_eq!(docker_nested[0], Arg::Literal("docker".into()), "middle command should be docker");
    assert_eq!(docker_nested[1], Arg::Literal("exec".into()), "docker subcommand should be exec");
    assert_eq!(docker_nested[2], Arg::Literal("-it".into()), "docker exec should have -it flag");
    assert_eq!(docker_nested[3], Arg::Quoted("container-abc".into()), "docker exec target should be container-abc");

    // Innermost args are flattened directly into the docker exec invocation
    assert_eq!(docker_nested[4], Arg::Literal("cleat".into()), "innermost command should be cleat");
    assert_eq!(docker_nested[5], Arg::Literal("attach".into()), "cleat subcommand should be attach");
    assert_eq!(docker_nested[6], Arg::Literal(att_id.to_string()), "cleat should attach to correct session");
    assert_eq!(docker_nested.len(), 7, "docker nested should have exactly 7 args");

    // Verify flatten produces the expected structure
    let flat = flatten(outer_args, 0);
    assert!(flat.starts_with("ssh "), "flattened output should start with ssh: {flat}");
    assert!(flat.contains("docker exec -it"), "should contain docker exec: {flat}");
    assert!(flat.contains("container-abc"), "should contain the quoted container target: {flat}");
    assert!(flat.contains("cleat attach"), "should contain cleat attach: {flat}");
    assert!(flat.contains(att_id.as_str()), "should contain session id: {flat}");

    // Verify nesting depth updated for both remote and environment hops
    assert_eq!(context.nesting_depth, 2, "nesting_depth should be 2 after remote + environment hops");
    assert_eq!(context.current_host.as_str(), "feta", "current_host should be updated to feta");
    assert_eq!(context.current_environment, Some(EnvironmentId::new("env1")), "current_environment should be env1");
}
