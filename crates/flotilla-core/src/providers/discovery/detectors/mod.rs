pub mod claude;
pub mod cmux;
pub mod codex;
pub mod generic;
pub mod git;

use generic::{parse_first_dotted_version, CommandDetector, EnvVarDetector};

use super::{HostDetector, RepoDetector};

pub fn default_host_detectors() -> Vec<Box<dyn HostDetector>> {
    vec![
        Box::new(CommandDetector::new("git", &["--version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("gh", &["--version"], parse_first_dotted_version)),
        Box::new(claude::ClaudeDetector),
        Box::new(codex::CodexAuthDetector),
        Box::new(EnvVarDetector::new("CURSOR_API_KEY")),
        Box::new(CommandDetector::new("agent", &["--version"], parse_first_dotted_version)),
        Box::new(cmux::CmuxDetector),
        Box::new(EnvVarDetector::new("TMUX")),
        Box::new(EnvVarDetector::new("ZELLIJ")),
        Box::new(EnvVarDetector::new("ZELLIJ_SESSION_NAME")),
        Box::new(CommandDetector::new("zellij", &["--version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("shpool", &["version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("gemini", &["--version"], parse_first_dotted_version)),
    ]
}

pub fn default_repo_detectors() -> Vec<Box<dyn RepoDetector>> {
    vec![Box::new(git::VcsRepoDetector), Box::new(git::RemoteHostDetector)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::{
        test_support::{DiscoveryMockRunner, TestEnvVars},
        EnvironmentAssertion,
    };

    #[test]
    fn default_host_detectors_non_empty() {
        assert!(!default_host_detectors().is_empty());
    }

    #[test]
    fn default_repo_detectors_non_empty() {
        assert!(!default_repo_detectors().is_empty());
    }

    #[tokio::test]
    async fn simple_env_var_detectors_are_table_driven() {
        let runner = DiscoveryMockRunner::builder().build();
        let cases = [
            ("cursor-env", "CURSOR_API_KEY", "cursor-secret"),
            ("tmux", "TMUX", "/tmp/tmux.sock,123,0"),
            ("zellij-env", "ZELLIJ", "0"),
            ("zellij-session", "ZELLIJ_SESSION_NAME", "my-session"),
        ];

        for (_detector_name, key, value) in cases {
            let detector = EnvVarDetector::new(key);
            let env = TestEnvVars::new([(key, value)]);
            let assertions = detector.detect(&runner, &env).await;

            assert!(matches!(
                assertions.as_slice(),
                [EnvironmentAssertion::EnvVarSet { key: found_key, value: found_value }]
                if found_key == key && found_value == value
            ));
        }
    }

    #[tokio::test]
    async fn simple_command_detectors_are_table_driven() {
        let cases: Vec<(&str, &str, &[&str], &str, Option<&str>)> = vec![
            ("git-binary", "git", &["--version"], "git version 2.43.0\n", Some("2.43.0")),
            ("gh-cli", "gh", &["--version"], "gh version 2.49.0\n", Some("2.49.0")),
            ("cursor-agent", "agent", &["--version"], "0.1.0\n", Some("0.1.0")),
            ("zellij-binary", "zellij", &["--version"], "zellij 0.40.1\n", Some("0.40.1")),
            ("shpool", "shpool", &["version"], "shpool 0.9.0\n", Some("0.9.0")),
            ("gemini", "gemini", &["--version"], "gemini 1.0.0\n", Some("1.0.0")),
        ];

        for (_detector_name, command, args, output, version) in cases {
            let runner = DiscoveryMockRunner::builder().on_run(command, args, Ok(output.into())).build();
            let detector = CommandDetector::new(command, args, parse_first_dotted_version);
            let assertions = detector.detect(&runner, &TestEnvVars::default()).await;

            assert!(matches!(
                assertions.as_slice(),
                [EnvironmentAssertion::BinaryAvailable { name, version: found_version, .. }]
                if name == command && found_version.as_deref() == version
            ));
        }
    }
}
