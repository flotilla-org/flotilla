use std::path::Path;

use async_trait::async_trait;

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
}

impl CommandDetector {
    pub fn new(command: &'static str, args: &'static [&'static str], version_parser: VersionParser) -> Self {
        Self { command, args, version_parser }
    }
}

#[async_trait]
impl HostDetector for CommandDetector {
    async fn detect(&self, runner: &dyn CommandRunner, _env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        run!(runner, self.command, self.args, Path::new("."))
            .ok()
            .map(|output| match (self.version_parser)(&output) {
                Some(version) => vec![EnvironmentAssertion::versioned_binary(self.command, self.command, version)],
                None => vec![EnvironmentAssertion::binary(self.command, self.command)],
            })
            .unwrap_or_default()
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
    use super::*;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars},
    };

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
}
