use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use std::{
    collections::VecDeque,
    future::Future,
    io::{self, Write},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tracing::instrument::WithSubscriber;

use flotilla_core::providers::*;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log capture lock should be healthy").write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Capture tracing for one future without changing the process-wide subscriber.
pub async fn capture_logs<F: Future>(level: tracing::Level, future: F) -> (F::Output, String) {
    let log_output = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(Arc::clone(&log_output));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(level)
        .with_writer(move || writer.clone())
        .finish();

    let output = future.with_subscriber(subscriber).await;
    let logs = String::from_utf8(log_output.lock().expect("log capture lock should be healthy").clone()).expect("logs should be utf-8");
    (output, logs)
}

pub type TimeoutCall = (String, Vec<String>, PathBuf, Duration);

/// Stub that proves decorators use the timeout seam rather than `run`.
pub struct TimeoutOnlyRunner {
    pub calls: std::sync::Mutex<Vec<TimeoutCall>>,
    result: Result<String, String>,
}

impl TimeoutOnlyRunner {
    pub fn new(result: Result<String, String>) -> Self {
        Self { calls: std::sync::Mutex::new(Vec::new()), result }
    }
}

#[async_trait]
impl CommandRunner for TimeoutOnlyRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        panic!("run called instead of run_with_timeout")
    }

    async fn run_with_timeout(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        _label: &ChannelLabel,
        timeout: Duration,
    ) -> Result<String, String> {
        self.calls.lock().expect("calls mutex").push((
            cmd.to_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd.to_path_buf(),
            timeout,
        ));
        self.result.clone()
    }

    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        panic!("run_output not expected")
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

/// A mock command runner that returns canned responses in order.
/// Each call to `run()` or `run_output()` pops the next response from the queue.
pub struct MockRunner {
    responses: std::sync::Mutex<VecDeque<Result<CommandOutput, String>>>,
    calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

impl MockRunner {
    pub fn new(responses: Vec<Result<String, String>>) -> Self {
        Self::with_outputs(
            responses
                .into_iter()
                .map(|response| {
                    Ok(match response {
                        Ok(stdout) => CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) },
                        Err(stderr) => CommandOutput { stdout: String::new(), stderr, exit_code: Some(1) },
                    })
                })
                .collect(),
        )
    }

    /// Queue raw subprocess outputs or spawn errors, preserving both output streams.
    /// `new` retains its legacy stderr-only unsuccessful-command semantics.
    pub fn with_outputs(responses: Vec<Result<CommandOutput, String>>) -> Self {
        Self { responses: std::sync::Mutex::new(responses.into()), calls: std::sync::Mutex::new(vec![]) }
    }

    /// Returns the number of unconsumed canned responses.
    pub fn remaining(&self) -> usize {
        self.responses.lock().expect("MockRunner responses mutex not poisoned").len()
    }

    /// Returns a snapshot of all recorded (cmd, args) calls made so far.
    pub fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait]
impl CommandRunner for MockRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let output = self.run_output(cmd, args, cwd, label).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|a| (*a).into()).collect()));
        self.responses.lock().expect("responses").pop_front().expect("MockRunner: no more responses")
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, _input: &[u8]) -> Result<String, String> {
        self.run(cmd, args, cwd, label).await
    }

    async fn run_to_file(&self, cmd: &str, args: &[&str], _cwd: &Path, _destination: &Path) -> Result<(), String> {
        self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
        Ok(())
    }

    async fn run_from_file(&self, cmd: &str, args: &[&str], _cwd: &Path, _source: &Path) -> Result<(), String> {
        self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
        Ok(())
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }

    async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), String> {
        Ok(())
    }
}

// #2586: the subprocess fake preserves both streams and distinguishes exit failure
// from spawn failure. Glue: explicit rows cover success, failure, empty output, and spawn errors.
#[tokio::test]
async fn mock_runner_preserves_outputs_and_order() {
    let runner = MockRunner::with_outputs(vec![
        Ok(CommandOutput { stdout: "headers/body".into(), stderr: "exit failure".into(), exit_code: Some(1) }),
        Err("spawn failure".into()),
        Ok(CommandOutput { stdout: "success".into(), stderr: "warning".into(), exit_code: Some(0) }),
        Ok(CommandOutput { stdout: "success".into(), stderr: "warning".into(), exit_code: Some(0) }),
        Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(1) }),
    ]);
    let label = ChannelLabel::Default;
    // Exit failure still exposes both streams through the raw-output API.
    let output = runner.run_output("gh", &["api"], Path::new("/"), &label).await.expect("output");
    assert_eq!((output.stdout.as_str(), output.stderr.as_str(), output.success()), ("headers/body", "exit failure", false));
    // A spawn failure has no command output.
    assert_eq!(runner.run_output("missing", &[], Path::new("/"), &label).await.err().as_deref(), Some("spawn failure"));
    // Successful raw output also preserves stderr.
    let output = runner.run_output("raw-ok", &[], Path::new("/"), &label).await.expect("successful output");
    assert_eq!((output.stdout.as_str(), output.stderr.as_str(), output.success()), ("success", "warning", true));
    // The convenience API selects stdout on success, stderr on failure.
    assert_eq!(runner.run("ok", &[], Path::new("/"), &label).await, Ok("success".into()));
    assert_eq!(runner.run("empty", &[], Path::new("/"), &label).await, Err(String::new()));
    assert_eq!(runner.remaining(), 0);
    assert_eq!(
        runner.calls(),
        vec![
            ("gh".into(), vec!["api".into()]),
            ("missing".into(), vec![]),
            ("raw-ok".into(), vec![]),
            ("ok".into(), vec![]),
            ("empty".into(), vec![])
        ]
    );
    let legacy = MockRunner::new(vec![Ok("legacy success".into()), Err("legacy failure".into())]);
    assert_eq!(legacy.run("ok", &[], Path::new("/"), &label).await, Ok("legacy success".into()));
    let output = legacy.run_output("fail", &[], Path::new("/"), &label).await.expect("legacy output");
    assert_eq!((output.stdout.as_str(), output.stderr.as_str(), output.success()), ("", "legacy failure", false));
}

// Exhausting the subprocess queue remains a hard failure, even for raw output calls.
#[tokio::test]
#[should_panic(expected = "MockRunner: no more responses")]
async fn mock_runner_rejects_unexpected_command() {
    MockRunner::with_outputs(vec![]).run_output("unexpected", &[], Path::new("/"), &ChannelLabel::Default).await.ok();
}

/// Build the path to a provider fixture file.
///
/// `provider_dir` is the subdirectory under `src/providers/` (e.g. `"vcs"`, `"change_request"`).
pub fn fixture_path(provider_dir: &str, name: &str) -> String {
    format!("{}/src/providers/{}/fixtures/{}", env!("CARGO_MANIFEST_DIR"), provider_dir, name)
}

#[tokio::test]
async fn process_runner_ensure_file_creates_parents_and_writes_when_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/dir/config.toml");
    let runner = ProcessCommandRunner;
    let ensured = runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(ensured, "hello = true\n");
    assert_eq!(on_disk, "hello = true\n");
}

#[tokio::test]
async fn process_runner_ensure_file_preserves_existing_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/dir/config.toml");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    std::fs::write(&path, "existing = true\n").expect("seed file");

    let runner = ProcessCommandRunner;
    let ensured = runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(ensured, "existing = true\n");
    assert_eq!(on_disk, "existing = true\n");
}

#[tokio::test]
async fn process_runner_write_file_replaces_existing_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/brief.md");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    std::fs::write(&path, "old brief").expect("seed file");

    let runner = ProcessCommandRunner;
    runner.write_file(&path, "new secret brief").await.expect("write_file");

    assert_eq!(std::fs::read_to_string(&path).expect("read back"), "new secret brief");
}

#[cfg(unix)]
#[tokio::test]
async fn process_runner_publishes_secret_with_requested_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/token");
    let runner = ProcessCommandRunner;
    runner.write_file_with_mode(&path, "secret", 0o600).await.expect("write secret");
    assert_eq!(std::fs::read_to_string(&path).expect("read token"), "secret");
    assert_eq!(std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn process_runner_run_with_input_drains_output_while_writing_stdin() {
    let runner = ProcessCommandRunner;
    let input = vec![b'x'; 131_072];

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runner.run_with_input(
            "sh",
            &["-c", "dd if=/dev/zero bs=131072 count=1 2>/dev/null; cat >/dev/null"],
            Path::new("/"),
            &ChannelLabel::Default,
            &input,
        ),
    )
    .await
    .expect("stdin writing and output draining must not deadlock")
    .expect("child command");

    assert_eq!(output.len(), 131_072);
}

#[cfg(unix)]
#[tokio::test]
async fn timed_out_command_kills_its_process_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_path = dir.path().join("child.pid");
    let pid_path_arg = pid_path.to_string_lossy();
    let runner = ProcessCommandRunner;
    let args = ["-c", "sleep 30 & echo $! > \"$1\"; wait", "sh", &pid_path_arg];
    let command = runner.run_with_timeout("sh", &args, Path::new("/"), &ChannelLabel::Default, std::time::Duration::from_secs(1));
    let result = command.await;
    assert!(result.expect_err("command must time out").contains("timed out"));
    let pid: i32 = std::fs::read_to_string(&pid_path).expect("child pid").trim().parse().expect("numeric pid");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            #[cfg(target_os = "linux")]
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit_once(')').and_then(|(_, tail)| tail.trim().chars().next()))
                == Some('Z');
            #[cfg(not(target_os = "linux"))]
            let zombie = false;
            if !alive || zombie {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("grandchild must terminate");
}

#[tokio::test]
async fn process_runner_long_lived_process_supports_lifecycle_contract() {
    let runner = ProcessCommandRunner;
    let mut process = runner.spawn_long_lived("sleep", &["30"], Path::new("/"), &ChannelLabel::Default).await.expect("spawn sleep");

    assert!(process.try_wait().expect("poll running child").is_none());
    process.kill().await.expect("kill child");
    let status = process.wait().await.expect("reap killed child");
    assert!(!status.success());
    assert!(process.try_wait().expect("poll reaped child").is_some());
}

#[tokio::test]
async fn process_runner_long_lived_process_drop_does_not_leak_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_path = dir.path().join("child.pid");
    let pid_path_arg = pid_path.to_string_lossy();
    let runner = ProcessCommandRunner;
    let process = runner
        .spawn_long_lived("sh", &["-c", "echo $$ > \"$1\"; exec sleep 30", "sh", &pid_path_arg], Path::new("/"), &ChannelLabel::Default)
        .await
        .expect("spawn tracked sleep");

    let pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&pid_path).await {
                let pid = contents.trim();
                if !pid.is_empty() {
                    break pid.to_string();
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child should publish pid");

    drop(process);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let status = tokio::process::Command::new("kill")
                .args(["-0", &pid])
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .expect("poll child pid");
            if !status.success() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped process must terminate its child");
}
