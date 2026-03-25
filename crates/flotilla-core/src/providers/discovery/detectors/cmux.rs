//! Cmux workspace manager host detector.
//!
//! Cmux is a macOS app bundle, so the binary lives at
//! `/Applications/cmux.app/Contents/Resources/bin/cmux` and is not normally on
//! PATH. The detector checks the `CMUX_SOCKET_PATH` env var, then probes
//! the binary on PATH, and finally falls back to the hardcoded app-bundle path.

use async_trait::async_trait;

use crate::providers::{
    discovery::{EnvVars, EnvironmentAssertion, HostDetector},
    CommandRunner,
};

/// Hardcoded path to the cmux binary inside the macOS app bundle.
const CMUX_APP_BUNDLE_BIN: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

/// Detects the cmux workspace manager.
pub struct CmuxDetector;

#[async_trait]
impl HostDetector for CmuxDetector {
    async fn detect(&self, runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion> {
        let mut assertions = Vec::new();

        // 1. Check CMUX_SOCKET_PATH env var — proves we're running inside cmux
        if let Some(value) = env.get("CMUX_SOCKET_PATH") {
            assertions.push(EnvironmentAssertion::env_var("CMUX_SOCKET_PATH", value.clone()));
            assertions.push(EnvironmentAssertion::socket("cmux", value));
        }

        // 2. Check if cmux is on PATH
        if runner.exists("cmux", &["list-sessions", "--format=json"]).await {
            assertions.push(EnvironmentAssertion::binary("cmux", "cmux"));
        } else {
            // 3. Fall back to the macOS app-bundle path — runs the binary to confirm
            //    it's functional, not just present on disk
            if runner.exists(CMUX_APP_BUNDLE_BIN, &["list-sessions", "--format=json"]).await {
                assertions.push(EnvironmentAssertion::binary("cmux", CMUX_APP_BUNDLE_BIN));
            }
        }

        assertions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        path_context::{DaemonHostPath, ExecutionEnvironmentPath},
        providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars},
    };

    #[tokio::test]
    async fn cmux_detector_with_socket_and_binary() {
        // Env var set + binary on PATH → EnvVarSet + SocketAvailable + BinaryAvailable
        let socket_path = "/tmp/cmux-test.sock";
        let runner = DiscoveryMockRunner::builder().tool_exists("cmux", true).build();
        let env = TestEnvVars::new([("CMUX_SOCKET_PATH", socket_path)]);
        let assertions = CmuxDetector.detect(&runner, &env).await;

        // Should have EnvVarSet, SocketAvailable, and BinaryAvailable
        assert_eq!(assertions.len(), 3);

        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::EnvVarSet { key, value }
            if key == "CMUX_SOCKET_PATH" && value == socket_path
        ));
        assert!(matches!(
            &assertions[1],
            EnvironmentAssertion::SocketAvailable { name, path }
            if name == "cmux" && *path == DaemonHostPath::new(socket_path)
        ));
        assert!(matches!(
            &assertions[2],
            EnvironmentAssertion::BinaryAvailable { name, path, .. }
            if name == "cmux" && *path == ExecutionEnvironmentPath::new("cmux")
        ));
    }

    #[tokio::test]
    async fn cmux_detector_binary_only() {
        let runner = DiscoveryMockRunner::builder().tool_exists("cmux", true).build();
        let assertions = CmuxDetector.detect(&runner, &TestEnvVars::default()).await;

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::BinaryAvailable { name, path, .. }
            if name == "cmux" && *path == ExecutionEnvironmentPath::new("cmux")
        ));
    }

    #[tokio::test]
    async fn cmux_detector_app_bundle_fallback() {
        // Not on PATH, but app-bundle check succeeds via runner
        let runner = DiscoveryMockRunner::builder().tool_exists("cmux", false).tool_exists(CMUX_APP_BUNDLE_BIN, true).build();
        let assertions = CmuxDetector.detect(&runner, &TestEnvVars::default()).await;

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::BinaryAvailable { name, path, .. }
            if name == "cmux" && *path == ExecutionEnvironmentPath::new(CMUX_APP_BUNDLE_BIN)
        ));
    }

    #[tokio::test]
    async fn cmux_detector_nothing() {
        // No env var, no binary on PATH, app-bundle check fails via runner
        let runner = DiscoveryMockRunner::builder().tool_exists("cmux", false).tool_exists(CMUX_APP_BUNDLE_BIN, false).build();
        let assertions = CmuxDetector.detect(&runner, &TestEnvVars::default()).await;

        assert!(assertions.is_empty());
    }
}
