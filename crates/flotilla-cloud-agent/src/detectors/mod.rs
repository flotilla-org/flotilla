pub mod claude;
pub mod codex;
pub mod cursor;
use flotilla_discovery_api::detectors::resolve_binary_path;
use flotilla_discovery_api::HostDetector;

/// Detect the Codex binary and retain its resolved executable path.
pub fn codex_binary_detector() -> impl HostDetector {
    binary_presence_detector("codex")
}

/// Discover a harness executable without running it against ambient config.
pub fn binary_presence_detector(binary: &'static str) -> impl HostDetector {
    BinaryPresenceDetector(binary)
}

/// Executable lookup has no harness/config side effects. Isolated fulfilment
/// observation separately supplies version compatibility facts.
struct BinaryPresenceDetector(&'static str);
#[async_trait::async_trait]
impl HostDetector for BinaryPresenceDetector {
    async fn detect(
        &self,
        runner: &dyn flotilla_provider_runtime::CommandRunner,
        _env: &dyn flotilla_provider_runtime::EnvVars,
    ) -> Vec<flotilla_discovery_api::EnvironmentAssertion> {
        resolve_binary_path(runner, self.0)
            .await
            .map(|path| vec![flotilla_discovery_api::EnvironmentAssertion::binary(self.0, path)])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use flotilla_discovery_api::EnvironmentAssertion;

    use super::*;
    use crate::testing::{DiscoveryMockRunner, TestEnvVars};

    // These detectors compose the shared discovery-api probes; the mock answers
    // only the exact commands that discovery has always issued.
    #[tokio::test]
    async fn codex_binary_detector_only_resolves_path() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "codex"], Ok("/tools/codex\n".into()))
            .build();

        let assertions = codex_binary_detector().detect(&runner, &TestEnvVars::default()).await;

        assert_eq!(assertions, vec![EnvironmentAssertion::binary("codex", "/tools/codex")]);
    }

    #[tokio::test]
    async fn cursor_detectors_report_agent_binary_and_api_key() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("sh", &["-c", "command -v \"$1\"", "flotilla-binary-discovery", "agent"], Ok("/tools/agent\n".into()))
            .build();
        let env = TestEnvVars::new([("CURSOR_API_KEY", "cursor-secret")]);

        assert_eq!(cursor::binary_detector().detect(&runner, &env).await, vec![EnvironmentAssertion::binary("agent", "/tools/agent")]);
        assert_eq!(
            cursor::api_key_detector().detect(&runner, &env).await,
            vec![EnvironmentAssertion::env_var("CURSOR_API_KEY", "cursor-secret")]
        );
    }
    // Process seam: no harness execution is allowed during generic discovery.
    struct PresenceRunner;
    #[async_trait::async_trait]
    impl flotilla_provider_runtime::CommandRunner for PresenceRunner {
        async fn run(
            &self,
            cmd: &str,
            args: &[&str],
            _: &std::path::Path,
            _: &flotilla_provider_runtime::ChannelLabel,
        ) -> Result<String, String> {
            assert_eq!(cmd, "sh", "generic discovery may only look up executables");
            assert_eq!(&args[..3], &["-c", "command -v \"$1\"", "flotilla-binary-discovery"]);
            Ok(format!("/tools/{}", args[3]))
        }
        async fn run_output(
            &self,
            _: &str,
            _: &[&str],
            _: &std::path::Path,
            _: &flotilla_provider_runtime::ChannelLabel,
        ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
            panic!("no harness invocation")
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            panic!("no harness invocation")
        }
    }

    // Glue: every generic harness detector uses the same lookup-only contract;
    // isolated fulfilment observations separately provide actual versions.
    #[tokio::test]
    async fn harness_discovery_never_invokes_harnesses() {
        let detectors: Vec<Box<dyn HostDetector>> = vec![
            Box::new(claude::ClaudeDetector),
            Box::new(codex_binary_detector()),
            Box::new(cursor::binary_detector()),
            Box::new(binary_presence_detector("gemini")),
        ];
        for detector in detectors {
            let facts = detector.detect(&PresenceRunner, &TestEnvVars::default()).await;
            assert!(matches!(facts.as_slice(), [EnvironmentAssertion::BinaryAvailable { version: None, .. }]));
        }
    }
}
