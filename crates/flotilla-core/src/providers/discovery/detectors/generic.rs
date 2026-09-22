use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::warn;

use crate::providers::{
    discovery::{EnvVars, EnvironmentAssertion, HostDetector},
    run, CommandRunner,
};

pub type VersionParser = fn(&str) -> Option<String>;

pub struct EnvVarDetector {
    key: &'static str,
}

impl EnvVarDetector {
    pub fn new(key: &'static str) -> Self {
        Self { key }
    }
}

#[async_trait]
impl HostDetector for EnvVarDetector {
    async fn detect(&self, _runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        env.get(self.key).map(|value| vec![EnvironmentAssertion::env_var(self.key, value)]).unwrap_or_default()
    }
}

pub struct CommandDetector {
    command: &'static str,
    args: &'static [&'static str],
    version_parser: VersionParser,
    path_mode: BinaryPathMode,
}

enum BinaryPathMode {
    CommandName,
    ResolveAbsolute,
}

impl CommandDetector {
    pub fn new(command: &'static str, args: &'static [&'static str], version_parser: VersionParser) -> Self {
        Self { command, args, version_parser, path_mode: BinaryPathMode::CommandName }
    }

    pub fn with_resolved_path(mut self) -> Self {
        self.path_mode = BinaryPathMode::ResolveAbsolute;
        self
    }
}

#[async_trait]
impl HostDetector for CommandDetector {
    async fn detect(&self, runner: &dyn CommandRunner, _env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        let Ok(output) = run!(runner, self.command, self.args, Path::new(".")) else {
            return Vec::new();
        };
        let path = match self.path_mode {
            BinaryPathMode::CommandName => PathBuf::from(self.command),
            BinaryPathMode::ResolveAbsolute => {
                let Some(path) = resolve_binary_path(runner, self.command).await else {
                    return Vec::new();
                };
                path
            }
        };
        match (self.version_parser)(&output) {
            Some(version) => vec![EnvironmentAssertion::versioned_binary(self.command, path, version)],
            None => vec![EnvironmentAssertion::binary(self.command, path)],
        }
    }
}

pub(super) async fn resolve_binary_path(runner: &dyn CommandRunner, command: &str) -> Option<PathBuf> {
    // Resolve inside the runner's environment: provisioned and remote runners
    // intentionally do not share the daemon process's PATH.
    let output = match runner
        .run(
            "sh",
            &["-c", "command -v \"$1\"", "flotilla-binary-discovery", command],
            Path::new("."),
            &crate::providers::ChannelLabel::Default,
        )
        .await
    {
        Ok(output) => output,
        Err(error) => {
            warn!(binary = command, %error, "agent adapter binary path resolution failed after successful version probe");
            return None;
        }
    };
    let path = PathBuf::from(output.trim());
    if path.is_absolute() {
        Some(path)
    } else {
        warn!(binary = command, resolved_path = %path.display(), "agent adapter binary path resolution returned a relative path after successful version probe");
        None
    }
}

pub fn parse_first_dotted_version(output: &str) -> Option<String> {
    let mut start = None;
    let mut dot_count = 0;
    let mut saw_digit = false;

    for (idx, ch) in output.char_indices() {
        if start.is_none() {
            if ch.is_ascii_digit() {
                start = Some(idx);
                saw_digit = true;
            }
            continue;
        }

        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }

        if ch == '.' {
            dot_count += 1;
            saw_digit = false;
            continue;
        }

        if dot_count > 0 && saw_digit {
            let start = start.expect("version start must exist");
            return Some(output[start..idx].to_string());
        }

        start = if ch.is_ascii_digit() { Some(idx) } else { None };
        dot_count = 0;
        saw_digit = ch.is_ascii_digit();
    }

    if let Some(start) = start {
        if dot_count > 0 && saw_digit {
            return Some(output[start..].trim().to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use tracing::instrument::WithSubscriber;

    use super::*;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars},
    };

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

    async fn capture_warn_logs<F: Future>(future: F) -> (F::Output, String) {
        let log_output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(Arc::clone(&log_output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();

        let output = future.with_subscriber(subscriber).await;
        let logs = String::from_utf8(log_output.lock().expect("log capture lock should be healthy").clone()).expect("logs should be utf-8");
        (output, logs)
    }

    #[test]
    fn parse_first_dotted_version_handles_supported_outputs() {
        let cases = [
            ("git version 2.43.0\n", Some("2.43.0")),
            ("gh version 2.49.0 (2024-05-13)\nhttps://github.com/cli/cli\n", Some("2.49.0")),
            ("1.0.20 (Claude Code)\n", Some("1.0.20")),
            ("zellij 0.40.1\n", Some("0.40.1")),
            ("0.1.0\n", Some("0.1.0")),
            ("no version here\n", None),
        ];

        for (output, expected) in cases {
            assert_eq!(parse_first_dotted_version(output).as_deref(), expected);
        }
    }

    #[tokio::test]
    async fn env_var_detector_reads_from_env_source() {
        let detector = EnvVarDetector::new("CURSOR_API_KEY");
        let runner = DiscoveryMockRunner::builder().build();
        let env = TestEnvVars::new([("CURSOR_API_KEY", "secret")]);

        let assertions = detector.detect(&runner, &env).await;

        assert!(matches!(
            assertions.as_slice(),
            [EnvironmentAssertion::EnvVarSet { key, value }]
            if key == "CURSOR_API_KEY" && value == "secret"
        ));
    }

    #[tokio::test]
    async fn command_detector_uses_command_name_for_assertion() {
        let detector = CommandDetector::new("gh", &["--version"], parse_first_dotted_version);
        let runner = DiscoveryMockRunner::builder().on_run("gh", &["--version"], Ok("gh version 2.49.0\n".into())).build();

        let assertions = detector.detect(&runner, &TestEnvVars::default()).await;

        assert!(matches!(
            assertions.as_slice(),
            [EnvironmentAssertion::BinaryAvailable { name, path, version }]
            if name == "gh"
                && *path == ExecutionEnvironmentPath::new("gh")
                && version.as_deref() == Some("2.49.0")
        ));
    }

    #[tokio::test]
    async fn resolved_path_detector_warns_when_lookup_fails_after_successful_probe() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 0.5.0\n".into()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Err("sh is unavailable".into()))
            .build();
        let (assertions, logs) = capture_warn_logs(detector.detect(&runner, &TestEnvVars::default())).await;

        assert!(assertions.is_empty());
        assert!(logs.contains("codex"), "warning should name the rejected binary: {logs}");
        assert!(logs.contains("sh is unavailable"), "warning should explain the resolution failure: {logs}");
    }

    #[tokio::test]
    async fn resolved_path_detector_warns_when_lookup_returns_relative_path() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 0.5.0\n".into()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("bin/codex\n".into()))
            .build();
        let (assertions, logs) = capture_warn_logs(detector.detect(&runner, &TestEnvVars::default())).await;

        assert!(assertions.is_empty());
        assert!(logs.contains("codex"), "warning should name the rejected binary: {logs}");
        assert!(logs.contains("bin/codex"), "warning should include the rejected relative path: {logs}");
    }

    #[tokio::test]
    async fn resolved_path_detector_keeps_missing_binary_quiet() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = DiscoveryMockRunner::builder().on_run("codex", &["--version"], Err("command not found".into())).build();
        let (assertions, logs) = capture_warn_logs(detector.detect(&runner, &TestEnvVars::default())).await;

        assert!(assertions.is_empty());
        assert!(logs.is_empty(), "ordinary missing-binary detection should remain quiet: {logs}");
    }
}
