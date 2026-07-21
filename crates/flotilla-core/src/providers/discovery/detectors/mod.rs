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
        Box::new(CommandDetector::new("codex", &["--version"], parse_first_dotted_version)),
        Box::new(EnvVarDetector::new("ANTHROPIC_API_KEY")),
        Box::new(EnvVarDetector::new("CURSOR_API_KEY")),
        Box::new(CommandDetector::new("agent", &["--version"], parse_first_dotted_version)),
        Box::new(cmux::CmuxDetector),
        Box::new(EnvVarDetector::new("TMUX")),
        Box::new(EnvVarDetector::new("ZELLIJ")),
        Box::new(EnvVarDetector::new("ZELLIJ_SESSION_NAME")),
        Box::new(CommandDetector::new("zellij", &["--version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("cleat", &["--version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("shpool", &["version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("gemini", &["--version"], parse_first_dotted_version)),
        Box::new(EnvVarDetector::new("TERM")),
        Box::new(EnvVarDetector::new("COLORTERM")),
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
    #[allow(clippy::type_complexity)]
    async fn simple_command_detectors_are_table_driven() {
        let cases: Vec<(&str, &str, &[&str], &str, Option<&str>)> = vec![
            ("git-binary", "git", &["--version"], "git version 2.43.0\n", Some("2.43.0")),
            ("gh-cli", "gh", &["--version"], "gh version 2.49.0\n", Some("2.49.0")),
            ("cursor-agent", "agent", &["--version"], "0.1.0\n", Some("0.1.0")),
            ("zellij-binary", "zellij", &["--version"], "zellij 0.40.1\n", Some("0.40.1")),
            ("cleat", "cleat", &["--version"], "cleat 0.1.0\n", Some("0.1.0")),
            ("shpool", "shpool", &["version"], "shpool 0.9.0\n", Some("0.9.0")),
            ("gemini", "gemini", &["--version"], "gemini 1.0.0\n", Some("1.0.0")),
            ("codex", "codex", &["--version"], "codex-cli 0.5.0\n", Some("0.5.0")),
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

    /// The default detector list must surface the binaries agent-adapter
    /// discovery keys on. The codex adapter was unregistrable on every real
    /// host because no default detector probed the `codex` binary — only its
    /// auth file (flotilla-org/flotilla#796 dogfood, 2026-07-21).
    #[tokio::test]
    async fn default_detectors_surface_agent_adapter_binaries() {
        use std::sync::Arc;

        use crate::providers::discovery::run_host_detectors;

        let runner = DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 0.5.0\n".into()))
            .on_run("claude", &["--version"], Ok("1.0.0 (Claude Code)\n".into()))
            .build();
        let bag = run_host_detectors(&default_host_detectors(), &runner, &TestEnvVars::default()).await;

        assert!(bag.find_binary("codex").is_some(), "codex binary must be detected for its agent adapter to register");
        let adapters = crate::agent_adapter::AgentAdapterRegistry::discover(&bag, Arc::new(runner));
        assert!(adapters.get("codex").is_some(), "codex agent adapter should register when the binary is present");
    }
}
