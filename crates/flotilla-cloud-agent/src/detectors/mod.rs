pub mod claude;
pub mod codex;
pub mod cursor;
use flotilla_discovery_api::detectors::{parse_first_dotted_version, CommandDetector};
use flotilla_discovery_api::HostDetector;

/// Detect the Codex binary and retain its resolved executable path.
pub fn codex_binary_detector() -> impl HostDetector {
    CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path()
}

#[cfg(test)]
mod tests {
    use flotilla_discovery_api::EnvironmentAssertion;

    use super::*;
    use crate::testing::{DiscoveryMockRunner, TestEnvVars};

    // These detectors compose the shared discovery-api probes; the mock answers
    // only the exact commands that discovery has always issued.
    #[tokio::test]
    async fn codex_binary_detector_probes_version_then_resolves_path() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("codex", &["--version"], Ok("codex-cli 0.5.0\n".into()))
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("/tools/codex\n".into()))
            .build();

        let assertions = codex_binary_detector().detect(&runner, &TestEnvVars::default()).await;

        assert_eq!(assertions, vec![EnvironmentAssertion::versioned_binary("codex", "/tools/codex", "0.5.0")]);
    }

    #[tokio::test]
    async fn cursor_detectors_report_agent_binary_and_api_key() {
        let runner = DiscoveryMockRunner::builder().on_run("agent", &["--version"], Ok("0.1.0\n".into())).build();
        let env = TestEnvVars::new([("CURSOR_API_KEY", "cursor-secret")]);

        assert_eq!(
            cursor::binary_detector().detect(&runner, &env).await,
            vec![EnvironmentAssertion::versioned_binary("agent", "agent", "0.1.0")]
        );
        assert_eq!(
            cursor::api_key_detector().detect(&runner, &env).await,
            vec![EnvironmentAssertion::env_var("CURSOR_API_KEY", "cursor-secret")]
        );
    }
}
