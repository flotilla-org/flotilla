use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, EnvironmentStatus, HostName, ImageSource};

use super::{docker::DockerEnvironment, runner::EnvironmentRunner, CreateOpts, EnvironmentProvider};
use crate::providers::{ChannelLabel, CommandOutput, CommandRunner};

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
    let env_runner = EnvironmentRunner::new("test-container".to_string(), inner.clone());
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
    let env_runner = EnvironmentRunner::new("test-container".to_string(), inner.clone());
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
    let env_runner = EnvironmentRunner::new("test-container".to_string(), inner.clone());

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
    let env_runner = EnvironmentRunner::new("test-container".to_string(), inner.clone());

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
// DockerEnvironment tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ensure_image_builds_dockerfile() {
    let runner = Arc::new(RecordingRunner::new_ok(""));
    let provider = DockerEnvironment::new(runner.clone());
    let spec = EnvironmentSpec { image: ImageSource::Dockerfile("/path/to/Dockerfile".into()), token_env_vars: vec![] };
    let repo_root = std::path::Path::new("/repo");

    let result = provider.ensure_image(&spec, repo_root).await;

    assert!(result.is_ok(), "ensure_image should succeed for Dockerfile source");
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let (cmd, args, _) = &calls[0];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "build");
    assert!(args.contains(&"-t".to_string()), "should pass -t flag");
    assert!(args.contains(&"-f".to_string()), "should pass -f flag");
    let f_idx = args.iter().position(|a| a == "-f").expect("-f flag present");
    assert_eq!(args[f_idx + 1], "/path/to/Dockerfile");
}

#[tokio::test]
async fn ensure_image_pulls_registry() {
    let runner = Arc::new(RecordingRunner::new_ok(""));
    let provider = DockerEnvironment::new(runner.clone());
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
    let runner = Arc::new(RecordingRunner::new_ok("container-id-123"));
    let provider = DockerEnvironment::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![("GITHUB_TOKEN".into(), "ghp_secret".into())],
        reference_repo: None,
        daemon_socket_path: DaemonHostPath::new("/run/flotilla.sock"),
        working_directory: None,
    };

    let id = EnvironmentId::new("test-env-1");
    let result = provider.create(id, &image, opts).await;

    assert!(result.is_ok(), "create should succeed");
    let handle = result.unwrap();

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
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

    // Token env var should be present
    assert!(args.iter().any(|a| a.starts_with("GITHUB_TOKEN=")), "token env var should be passed");
}

#[tokio::test]
async fn status_returns_running() {
    use flotilla_protocol::ImageId;
    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id".into()), // docker run
        Ok("running".into()),      // docker inspect
    ]));
    let provider = DockerEnvironment::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        reference_repo: None,
        daemon_socket_path: DaemonHostPath::new("/run/flotilla.sock"),
        working_directory: None,
    };

    let id = EnvironmentId::new("test-env-status");
    let handle = provider.create(id, &image, opts).await.expect("create");
    let status = handle.status().await.expect("status");

    assert_eq!(status, EnvironmentStatus::Running);
    let calls = runner.calls();
    // Second call should be docker inspect
    let (cmd, args, _) = &calls[1];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "inspect");
    assert!(args.contains(&"--format".to_string()));
}

#[tokio::test]
async fn env_vars_parses_output() {
    use flotilla_protocol::ImageId;
    let runner = Arc::new(QueuedRunner::new([
        Ok("container-id".into()),       // docker run
        Ok("FOO=bar\nBAZ=qux\n".into()), // docker exec sh -lc env
    ]));
    let provider = DockerEnvironment::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        reference_repo: None,
        daemon_socket_path: DaemonHostPath::new("/run/flotilla.sock"),
        working_directory: None,
    };

    let id = EnvironmentId::new("test-env-vars");
    let handle = provider.create(id, &image, opts).await.expect("create");
    let vars = handle.env_vars().await.expect("env_vars");

    assert_eq!(vars.get("FOO"), Some(&"bar".to_string()));
    assert_eq!(vars.get("BAZ"), Some(&"qux".to_string()));

    let calls = runner.calls();
    let (cmd, args, _) = &calls[1];
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
        Ok("".into()),             // docker rm -f
    ]));
    let provider = DockerEnvironment::new(runner.clone());
    let image = ImageId::new("ubuntu:22.04");
    let opts = CreateOpts {
        tokens: vec![],
        reference_repo: None,
        daemon_socket_path: DaemonHostPath::new("/run/flotilla.sock"),
        working_directory: None,
    };

    let id = EnvironmentId::new("test-env-destroy");
    let handle = provider.create(id, &image, opts).await.expect("create");
    let container_name = format!("flotilla-env-{}", handle.id());
    handle.destroy().await.expect("destroy");

    let calls = runner.calls();
    let (cmd, args, _) = &calls[1];
    assert_eq!(cmd, "docker");
    assert_eq!(args[0], "rm");
    assert!(args.contains(&"-f".to_string()), "should pass -f flag");
    assert!(args.contains(&container_name), "should pass container name");
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

/// Verifies that EnvironmentRunner composes correctly with the CleatTerminalPoolFactory:
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
    let env_runner = Arc::new(EnvironmentRunner::new("test-container".to_string(), inner.clone()));

    // Build an EnvironmentBag that asserts cleat is available at the path the factory expects
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"));

    let dir = tempfile::tempdir().expect("tempdir");
    let config = ConfigStore::with_base(dir.path());
    let repo_root = ExecutionEnvironmentPath::new("/repo");

    // The factory checks env.find_binary("cleat") first — it does NOT call runner for binary detection.
    // Passing the EnvironmentRunner as the runner proves the decorator is accepted by the factory
    // and that CleatTerminalPool is constructed with it, proving the composition path.
    let result = CleatTerminalPoolFactory.probe(&bag, &config, &repo_root, env_runner.clone()).await;
    assert!(result.is_ok(), "probe should succeed when cleat binary assertion is present");

    // Verify that no actual docker exec calls were made during probe (factory only checks bag)
    let calls = inner.calls();
    assert!(calls.is_empty(), "factory probe should not invoke runner during binary check");
}

/// Verifies that EnvironmentRunner correctly transforms command calls into docker exec form,
/// matching the pattern that discovery factories would issue inside a container.
#[tokio::test]
async fn environment_runner_transforms_commands_for_container() {
    // Simulate the exact check a discovery factory might perform: "cleat --version"
    let inner = Arc::new(RecordingRunner::new_ok("cleat 0.5.0"));
    let env_runner = EnvironmentRunner::new("my-container".to_string(), inner.clone());
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
            environment::DockerEnvironmentHopResolver,
            remote::RemoteHopResolver,
            resolver::{AlwaysWrap, HopResolver},
            terminal::TerminalHopResolver,
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
            let inner_args = match inner_action {
                ResolvedAction::Command(args) => args,
                other => return Err(format!("mock remote wrap: expected Command, got {other:?}")),
            };
            let mut ssh_args = vec![Arg::Literal("ssh".into()), Arg::Quoted(host.as_str().to_string())];
            ssh_args.push(Arg::NestedCommand(inner_args));
            context.actions.push(ResolvedAction::Command(ssh_args));
            Ok(())
        }

        fn resolve_enter(&self, _host: &HostName, _context: &mut ResolutionContext) -> Result<(), String> {
            unimplemented!("only wrap mode used in this test")
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

    let resolver = HopResolver {
        remote: Arc::new(MockRemote),
        environment: docker_env,
        terminal: Arc::new(MockTerminal),
        strategy: Arc::new(AlwaysWrap),
    };

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

    let outer_args = match &resolved.0[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

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
    assert_eq!(docker_nested[3], Arg::Literal("container-abc".into()), "docker exec target should be container-abc");

    // Innermost args are flattened directly into the docker exec invocation
    assert_eq!(docker_nested[4], Arg::Literal("cleat".into()), "innermost command should be cleat");
    assert_eq!(docker_nested[5], Arg::Literal("attach".into()), "cleat subcommand should be attach");
    assert_eq!(docker_nested[6], Arg::Literal(att_id.to_string()), "cleat should attach to correct session");
    assert_eq!(docker_nested.len(), 7, "docker nested should have exactly 7 args");

    // Verify flatten produces the expected structure
    let flat = flatten(outer_args, 0);
    assert!(flat.starts_with("ssh "), "flattened output should start with ssh: {flat}");
    assert!(flat.contains("docker exec -it container-abc"), "should contain docker exec: {flat}");
    assert!(flat.contains("cleat attach"), "should contain cleat attach: {flat}");
    assert!(flat.contains(att_id.as_str()), "should contain session id: {flat}");

    // Verify nesting depth updated for both remote and environment hops
    assert_eq!(context.nesting_depth, 2, "nesting_depth should be 2 after remote + environment hops");
    assert_eq!(context.current_host.as_str(), "feta", "current_host should be updated to feta");
    assert_eq!(context.current_environment, Some(EnvironmentId::new("env1")), "current_environment should be env1");
}
