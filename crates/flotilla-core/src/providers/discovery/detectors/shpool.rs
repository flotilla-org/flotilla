//! Shpool session manager host detector.
//!
//! Checks whether the `shpool` binary is available on the host.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, HostDetector};
use crate::providers::{run, CommandRunner};

/// Detects the shpool session manager binary.
pub struct ShpoolDetector;

#[async_trait]
impl HostDetector for ShpoolDetector {
    fn name(&self) -> &str {
        "shpool"
    }

    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        // Single call: proves binary exists and captures version
        if let Ok(output) = run!(runner, "shpool", &["version"], Path::new(".")) {
            let version = {
                let trimmed = output.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            };
            vec![EnvironmentAssertion::BinaryAvailable {
                name: "shpool".into(),
                path: PathBuf::from("shpool"),
                version,
            }]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    #[tokio::test]
    async fn shpool_detector_found() {
        let runner = DiscoveryMockRunner::builder()
            .on_run("shpool", &["version"], Ok("shpool 0.6.2\n".into()))
            .build();
        let assertions = ShpoolDetector.detect(&runner).await;

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::BinaryAvailable { name, path, version }
            if name == "shpool" && path == &PathBuf::from("shpool") && version.as_deref() == Some("shpool 0.6.2")
        ));
    }

    #[tokio::test]
    async fn shpool_detector_not_found() {
        // No on_run configured → run! returns Err → empty
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = ShpoolDetector.detect(&runner).await;

        assert!(assertions.is_empty());
    }
}
