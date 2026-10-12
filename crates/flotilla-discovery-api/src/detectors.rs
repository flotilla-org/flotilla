//! Generic host-probe detectors shared by discovery assembly and the provider
//! type crates: environment-variable presence, command version probes, binary
//! path resolution inside the runner's environment, and version parsing.
//! Provider crates compose these for their type-specific detectors; one
//! implementation keeps command arguments, path resolution, warnings and
//! version parsing identical everywhere (#3049).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use flotilla_provider_runtime::run;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;
use tracing::warn;

use crate::{EnvironmentAssertion, HostDetector};

/// Extracts a version from a successful probe's standard output.
pub type VersionParser = fn(&str) -> Option<String>;

/// Asserts an environment variable's value when the injected environment sets it.
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

/// Runs `command args` from `/` and asserts the binary when the probe succeeds.
/// The asserted path is the command name unless [`CommandDetector::with_resolved_path`]
/// requests the absolute executable path resolved inside the runner's environment.
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
        let Ok(output) = run!(runner, self.command, self.args, Path::new("/")) else {
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

/// Resolve a command that already passed its probe to the absolute executable
/// path inside the runner's environment.
///
/// This is deliberately a second runner call, not a `which`-style lookup on
/// the daemon host: the runner may be remote or provisioned, so the daemon's
/// own `PATH` can name a different binary or none.
pub async fn resolve_binary_path(runner: &dyn CommandRunner, command: &str) -> Option<PathBuf> {
    // Resolve inside the runner's environment: provisioned and remote runners
    // intentionally do not share the daemon process's PATH.
    let output = match runner
        .run(
            "sh",
            &["-c", "command -v \"$1\"", "flotilla-binary-discovery", command],
            Path::new("/"),
            &flotilla_provider_runtime::ChannelLabel::Default,
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

/// The first token of digits containing at least one dot, such as `2.43.0`.
pub fn parse_first_dotted_version(output: &str) -> Option<String> {
    let mut start = None;
    let mut dot_count = 0;
    let mut saw_digit = false;

    for (idx, ch) in output.char_indices() {
        let Some(begin) = start else {
            if ch.is_ascii_digit() {
                start = Some(idx);
                saw_digit = true;
            }
            continue;
        };

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
            return Some(output[begin..idx].to_string());
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
    use std::collections::HashMap;

    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_replay_testkit::testing::{capture_logs, MockRunner};

    use super::*;

    const RESOLVE_ARGS: [&str; 4] = ["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"];

    #[derive(Default)]
    struct TestEnvVars(HashMap<String, String>);

    impl TestEnvVars {
        fn new<const N: usize>(values: [(&str, &str); N]) -> Self {
            Self(values.into_iter().map(|(key, value)| (key.to_string(), value.to_string())).collect())
        }
    }

    impl EnvVars for TestEnvVars {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn call(cmd: &str, args: &[&str]) -> (String, Vec<String>) {
        (cmd.to_string(), args.iter().map(|arg| arg.to_string()).collect())
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
        let runner = MockRunner::new(vec![]);
        let env = TestEnvVars::new([("CURSOR_API_KEY", "secret")]);

        let assertions = detector.detect(&runner, &env).await;

        assert_eq!(assertions, vec![EnvironmentAssertion::env_var("CURSOR_API_KEY", "secret")]);
        assert!(detector.detect(&runner, &TestEnvVars::default()).await.is_empty());
        assert!(runner.calls().is_empty(), "environment detection never runs commands");
    }

    #[tokio::test]
    async fn command_detector_uses_command_name_for_assertion() {
        let detector = CommandDetector::new("gh", &["--version"], parse_first_dotted_version);
        let runner = MockRunner::new(vec![Ok("gh version 2.49.0\n".into())]);

        let assertions = detector.detect(&runner, &TestEnvVars::default()).await;

        assert!(matches!(
            assertions.as_slice(),
            [EnvironmentAssertion::BinaryAvailable { name, path, version }]
            if name == "gh"
                && *path == ExecutionEnvironmentPath::new("gh")
                && version.as_deref() == Some("2.49.0")
        ));
        assert_eq!(runner.calls(), vec![call("gh", &["--version"])]);
    }

    #[tokio::test]
    async fn command_detector_without_version_asserts_unversioned_binary() {
        let detector = CommandDetector::new("cleat", &["--version"], parse_first_dotted_version);
        let runner = MockRunner::new(vec![Ok("cleat development build\n".into())]);

        assert_eq!(detector.detect(&runner, &TestEnvVars::default()).await, vec![EnvironmentAssertion::binary("cleat", "cleat")]);
    }

    #[tokio::test]
    async fn resolved_path_detector_issues_probe_then_resolution_in_runner_environment() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = MockRunner::new(vec![Ok("codex-cli 0.5.0\n".into()), Ok("/tools/codex\n".into())]);

        let assertions = detector.detect(&runner, &TestEnvVars::default()).await;

        assert_eq!(assertions, vec![EnvironmentAssertion::versioned_binary("codex", "/tools/codex", "0.5.0")]);
        assert_eq!(runner.calls(), vec![call("codex", &["--version"]), call("sh", &RESOLVE_ARGS)]);
    }

    #[tokio::test]
    async fn resolved_path_detector_warns_when_lookup_fails_after_successful_probe() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = MockRunner::new(vec![Ok("codex-cli 0.5.0\n".into()), Err("sh is unavailable".into())]);
        let (assertions, logs) = capture_logs(tracing::Level::WARN, detector.detect(&runner, &TestEnvVars::default())).await;

        assert!(assertions.is_empty());
        assert!(logs.contains("codex"), "warning should name the rejected binary: {logs}");
        assert!(logs.contains("sh is unavailable"), "warning should explain the resolution failure: {logs}");
    }

    #[tokio::test]
    async fn resolved_path_detector_warns_when_lookup_returns_relative_path() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = MockRunner::new(vec![Ok("codex-cli 0.5.0\n".into()), Ok("bin/codex\n".into())]);
        let (assertions, logs) = capture_logs(tracing::Level::WARN, detector.detect(&runner, &TestEnvVars::default())).await;

        assert!(assertions.is_empty());
        assert!(logs.contains("codex"), "warning should name the rejected binary: {logs}");
        assert!(logs.contains("bin/codex"), "warning should include the rejected relative path: {logs}");
    }

    #[tokio::test]
    async fn resolved_path_detector_keeps_missing_binary_quiet() {
        let detector = CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path();
        let runner = MockRunner::new(vec![Err("command not found".into())]);
        let (assertions, logs) = capture_logs(tracing::Level::WARN, detector.detect(&runner, &TestEnvVars::default())).await;

        assert!(assertions.is_empty());
        assert!(logs.is_empty(), "ordinary missing-binary detection should remain quiet: {logs}");
        assert_eq!(runner.calls(), vec![call("codex", &["--version"])], "a failed probe never resolves a path");
    }
}
