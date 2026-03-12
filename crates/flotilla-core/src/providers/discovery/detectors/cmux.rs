//! Cmux workspace manager host detector.
//!
//! Cmux is a macOS app bundle, so the binary lives at
//! `/Applications/cmux.app/Contents/Resources/bin/cmux` and is not normally on
//! PATH. The detector checks the `CMUX_SOCKET_PATH` env var, then probes
//! the binary on PATH, and finally falls back to the hardcoded app-bundle path.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, HostDetector};
use crate::providers::CommandRunner;

/// Hardcoded path to the cmux binary inside the macOS app bundle.
const CMUX_APP_BUNDLE_BIN: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

/// Detects the cmux workspace manager.
pub struct CmuxDetector;

#[async_trait]
impl HostDetector for CmuxDetector {
    fn name(&self) -> &str {
        "cmux"
    }

    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        let mut assertions = Vec::new();

        // 1. Check CMUX_SOCKET_PATH env var — proves we're running inside cmux
        if let Ok(value) = std::env::var("CMUX_SOCKET_PATH") {
            assertions.push(EnvironmentAssertion::EnvVarSet {
                key: "CMUX_SOCKET_PATH".into(),
                value: value.clone(),
            });
            assertions.push(EnvironmentAssertion::SocketAvailable {
                name: "cmux".into(),
                path: PathBuf::from(value),
            });
        }

        // 2. Check if cmux is on PATH
        if runner
            .exists("cmux", &["list-sessions", "--format=json"])
            .await
        {
            assertions.push(EnvironmentAssertion::BinaryAvailable {
                name: "cmux".into(),
                path: PathBuf::from("cmux"),
                version: None,
            });
        } else {
            // 3. Fall back to the hardcoded app-bundle path
            let app_bundle_path = Path::new(CMUX_APP_BUNDLE_BIN);
            if app_bundle_path.exists() {
                assertions.push(EnvironmentAssertion::BinaryAvailable {
                    name: "cmux".into(),
                    path: app_bundle_path.to_path_buf(),
                    version: None,
                });
            }
        }

        assertions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    #[tokio::test]
    async fn cmux_detector_with_socket_and_binary() {
        // Env var set + binary on PATH → EnvVarSet + SocketAvailable + BinaryAvailable
        let socket_path = "/tmp/cmux-test.sock";
        // SAFETY: process-global env var mutation; tests in this module run serially
        // via cargo test's default threading model for each test binary.
        unsafe {
            std::env::set_var("CMUX_SOCKET_PATH", socket_path);
        }

        let runner = DiscoveryMockRunner::builder()
            .tool_exists("cmux", true)
            .build();
        let assertions = CmuxDetector.detect(&runner).await;

        unsafe {
            std::env::remove_var("CMUX_SOCKET_PATH");
        }

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
            if name == "cmux" && path == Path::new(socket_path)
        ));
        assert!(matches!(
            &assertions[2],
            EnvironmentAssertion::BinaryAvailable { name, path, .. }
            if name == "cmux" && path == &PathBuf::from("cmux")
        ));
    }

    #[tokio::test]
    async fn cmux_detector_binary_only() {
        // No env var, binary on PATH → BinaryAvailable only
        unsafe {
            std::env::remove_var("CMUX_SOCKET_PATH");
        }

        let runner = DiscoveryMockRunner::builder()
            .tool_exists("cmux", true)
            .build();
        let assertions = CmuxDetector.detect(&runner).await;

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::BinaryAvailable { name, path, .. }
            if name == "cmux" && path == &PathBuf::from("cmux")
        ));
    }

    #[tokio::test]
    async fn cmux_detector_nothing() {
        // No env var, no binary on PATH, and the app-bundle path likely doesn't
        // exist in CI — assert empty (or just BinaryAvailable if the app is
        // installed on this machine).
        unsafe {
            std::env::remove_var("CMUX_SOCKET_PATH");
        }

        let runner = DiscoveryMockRunner::builder()
            .tool_exists("cmux", false)
            .build();
        let assertions = CmuxDetector.detect(&runner).await;

        // No env var assertions should be present
        assert!(!assertions
            .iter()
            .any(|a| matches!(a, EnvironmentAssertion::EnvVarSet { .. })));
        assert!(!assertions
            .iter()
            .any(|a| matches!(a, EnvironmentAssertion::SocketAvailable { .. })));

        // If the app-bundle path exists on this machine, we get a BinaryAvailable;
        // otherwise empty. Both are correct.
        for a in &assertions {
            match a {
                EnvironmentAssertion::BinaryAvailable { name, path, .. } => {
                    assert_eq!(name, "cmux");
                    assert_eq!(path, &PathBuf::from(CMUX_APP_BUNDLE_BIN));
                }
                other => panic!("unexpected assertion: {other:?}"),
            }
        }
    }
}
