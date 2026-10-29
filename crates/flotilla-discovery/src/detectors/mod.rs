use flotilla_cloud_agent::detectors::{self as cloud_agent, claude, codex, cursor};
use flotilla_discovery_api::host_environment_keys;
use flotilla_discovery_api::{HostDetector, RepoDetector};

use flotilla_discovery_api::detectors::{parse_first_dotted_version, CommandDetector, EnvVarDetector};

use flotilla_vcs::providers::detection::{git_binary_detector, VcsRepoDetector};

pub fn default_host_detectors() -> Vec<Box<dyn HostDetector>> {
    let mut detectors: Vec<Box<dyn HostDetector>> = vec![
        Box::new(git_binary_detector()),
        Box::new(CommandDetector::new("curl", &["--version"], parse_first_dotted_version)),
        Box::new(CommandDetector::new("gh", &["--version"], parse_first_dotted_version)),
        Box::new(claude::ClaudeDetector),
        Box::new(codex::CodexAuthDetector),
        Box::new(cloud_agent::codex_binary_detector()),
        Box::new(EnvVarDetector::new("CLEAT_RUNTIME_DIR")),
        Box::new(EnvVarDetector::new("CLEAT_DAEMON")),
        Box::new(EnvVarDetector::new("CODEX_HOME")),
        Box::new(EnvVarDetector::new("FLOTILLA_FLEET_CANARY")),
        Box::new(EnvVarDetector::new("ANTHROPIC_API_KEY")),
        Box::new(cursor::api_key_detector()),
        Box::new(cursor::binary_detector()),
        Box::new(CommandDetector::new("cleat", &["--version"], parse_first_dotted_version)),
        Box::new(cloud_agent::binary_presence_detector("gemini")),
        Box::new(EnvVarDetector::new("TERM")),
        Box::new(EnvVarDetector::new("COLORTERM")),
    ];
    detectors.extend(host_environment_keys().iter().map(|&key| Box::new(EnvVarDetector::new(key)) as Box<dyn HostDetector>));
    detectors
}

pub fn default_repo_detectors() -> Vec<Box<dyn RepoDetector>> {
    vec![Box::new(VcsRepoDetector)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_host_detectors;
    use crate::testing::DiscoveryMockRunner;
    use crate::testing::TestEnvVars;
    use flotilla_discovery_api::EnvironmentAssertion;

    #[test]
    fn default_host_detectors_non_empty() {
        assert!(!default_host_detectors().is_empty());
    }

    #[test]
    fn default_repo_detectors_non_empty() {
        assert!(!default_repo_detectors().is_empty());
    }

    // The default list's environment detectors report exactly the injected value.
    #[tokio::test]
    async fn default_env_var_detectors_are_table_driven() {
        let runner = DiscoveryMockRunner::builder().build();
        let cases = [("CURSOR_API_KEY", "cursor-secret"), ("CLEAT_DAEMON", "main")];

        for (key, value) in cases {
            let bag = run_host_detectors(&default_host_detectors(), &runner, &TestEnvVars::new([(key, value)])).await;

            assert_eq!(bag.assertions(), [EnvironmentAssertion::env_var(key, value)], "{key}");
        }
    }

    // The default list issues each command probe with its historical arguments;
    // the mock answers only that exact command, so any argument drift fails.
    #[tokio::test]
    #[allow(clippy::type_complexity)]
    async fn default_command_detectors_are_table_driven() {
        let resolve = |command: &'static str| ("sh", vec!["-c", "command -v \"$1\"", "flotilla-binary-discovery", command]);
        let cases: Vec<(&str, &[&str], &str, Option<(&str, Vec<&str>)>, &str, &str)> = vec![
            ("git", &["--version"], "git version 2.43.0\n", None, "git", "2.43.0"),
            ("curl", &["--version"], "curl 8.7.1 (arm64-apple-darwin)\n", None, "curl", "8.7.1"),
            ("gh", &["--version"], "gh version 2.49.0\n", None, "gh", "2.49.0"),
            ("agent", &["--version"], "0.1.0\n", Some(resolve("agent")), "/tools/agent", "0.1.0"),
            ("cleat", &["--version"], "cleat 0.1.0\n", None, "cleat", "0.1.0"),
            ("gemini", &["--version"], "gemini 1.0.0\n", Some(resolve("gemini")), "/tools/gemini", "1.0.0"),
            ("codex", &["--version"], "codex-cli 0.5.0\n", Some(resolve("codex")), "/tools/codex", "0.5.0"),
            ("claude", &["--version"], "1.0.20 (Claude Code)\n", Some(resolve("claude")), "/tools/claude", "1.0.20"),
        ];

        for (command, args, output, resolution, path, version) in cases {
            let mut runner = DiscoveryMockRunner::builder().on_run(command, args, Ok(output.into()));
            if let Some((shell, shell_args)) = resolution {
                runner = runner.on_run(shell, &shell_args, Ok(format!("{path}\n")));
            }
            let bag = run_host_detectors(&default_host_detectors(), &runner.build(), &TestEnvVars::default()).await;

            let expected = if ["claude", "codex", "agent", "gemini"].contains(&command) {
                EnvironmentAssertion::binary(command, path)
            } else {
                EnvironmentAssertion::versioned_binary(command, path, version)
            };
            assert_eq!(bag.assertions(), [expected], "{command}");
        }
    }

    /// The default detector list must surface the binaries agent-adapter
    /// discovery keys on. The codex adapter was unregistrable on every real
    /// host because no default detector probed the `codex` binary — only its
    /// auth file (flotilla-org/flotilla#796 dogfood, 2026-07-21).
    #[tokio::test]
    async fn default_detectors_surface_agent_adapter_binaries() {
        use std::sync::Arc;

        let runner = DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 0.5.0\n".into()))
            .on_run("claude", &["--version"], Ok("1.0.0 (Claude Code)\n".into()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("/tools/codex\n".into()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "claude"], Ok("/tools/claude\n".into()))
            .build();
        let bag = run_host_detectors(&default_host_detectors(), &runner, &TestEnvVars::new([("HOME", "/mock/home")])).await;

        assert!(bag.find_binary("codex").is_some(), "codex binary must be detected for its agent adapter to register");
        assert_eq!(bag.find_env_var("HOME"), Some("/mock/home"), "Codex config discovery needs the detected home directory");
        let adapters = flotilla_agent::adapter::AgentAdapterRegistry::discover(&bag, Arc::new(runner));
        let codex = adapters.get("codex").expect("codex agent adapter should register when the binary is present");
        codex
            .prepare(
                &flotilla_paths::path_context::ExecutionEnvironmentPath::new("/workspace"),
                &flotilla_resources::TerminalBrief {
                    artifact_digest: None,
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "Implement the issue.".into(),
                    copies: Vec::new(),
                },
            )
            .await
            .expect("production-shaped discovery should supply Codex config location");
    }
}
