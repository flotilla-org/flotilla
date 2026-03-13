//! Environment-variable-based host detectors for multiplexers.
//!
//! Contains `ZellijDetector` which checks the `ZELLIJ` env var and binary.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, HostDetector};
use crate::providers::{run, CommandRunner};

/// Detects the Zellij terminal multiplexer via env var and binary.
pub struct ZellijDetector;

#[async_trait]
impl HostDetector for ZellijDetector {
    fn name(&self) -> &str {
        "zellij"
    }

    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        let mut assertions = Vec::new();

        // Check ZELLIJ env var — proves we're running inside zellij
        if let Ok(value) = std::env::var("ZELLIJ") {
            assertions.push(EnvironmentAssertion::EnvVarSet {
                key: "ZELLIJ".into(),
                value,
            });
        }

        // Check binary and extract version for compatibility checking
        if runner.exists("zellij", &["--version"]).await {
            let version = run!(runner, "zellij", &["--version"], Path::new("."))
                .ok()
                .and_then(|output| {
                    // Output format: "zellij 0.40.1" or similar
                    let trimmed = output.trim();
                    trimmed
                        .strip_prefix("zellij ")
                        .map(|v| v.to_string())
                        .or_else(|| Some(trimmed.to_string()))
                });
            assertions.push(EnvironmentAssertion::BinaryAvailable {
                name: "zellij".into(),
                path: PathBuf::from("zellij"),
                version,
            });
        }

        assertions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    // Tests that manipulate the ZELLIJ env var must not run concurrently.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn zellij_detector_inside() {
        let _guard = ENV_LOCK.lock().expect("env lock");

        // Env var set + binary found → both assertions
        unsafe {
            std::env::set_var("ZELLIJ", "0");
        }

        let runner = DiscoveryMockRunner::builder()
            .tool_exists("zellij", true)
            .on_run("zellij", &["--version"], Ok("zellij 0.40.1\n".into()))
            .build();
        let assertions = ZellijDetector.detect(&runner).await;

        unsafe {
            std::env::remove_var("ZELLIJ");
        }

        assert_eq!(assertions.len(), 2);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::EnvVarSet { key, value }
            if key == "ZELLIJ" && value == "0"
        ));
        assert!(matches!(
            &assertions[1],
            EnvironmentAssertion::BinaryAvailable { name, version, .. }
            if name == "zellij" && version.as_deref() == Some("0.40.1")
        ));
    }

    #[tokio::test]
    async fn zellij_detector_not_inside_binary_found() {
        let _guard = ENV_LOCK.lock().expect("env lock");

        // Env var not set, but binary is available → BinaryAvailable only
        unsafe {
            std::env::remove_var("ZELLIJ");
        }

        let runner = DiscoveryMockRunner::builder()
            .tool_exists("zellij", true)
            .on_run("zellij", &["--version"], Ok("zellij 0.40.1\n".into()))
            .build();
        let assertions = ZellijDetector.detect(&runner).await;

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::BinaryAvailable { name, version, .. }
            if name == "zellij" && version.as_deref() == Some("0.40.1")
        ));
    }

    #[tokio::test]
    async fn zellij_detector_not_inside_no_binary() {
        let _guard = ENV_LOCK.lock().expect("env lock");

        // Env var not set, binary not found → empty
        unsafe {
            std::env::remove_var("ZELLIJ");
        }

        let runner = DiscoveryMockRunner::builder()
            .tool_exists("zellij", false)
            .build();
        let assertions = ZellijDetector.detect(&runner).await;

        assert!(assertions.is_empty());
    }
}
