//! Tmux multiplexer host detector.
//!
//! Checks the `TMUX` env var to determine whether we are running inside a tmux
//! session.

use async_trait::async_trait;

use crate::providers::discovery::{EnvironmentAssertion, HostDetector};
use crate::providers::CommandRunner;

/// Detects whether the current process is running inside tmux.
pub struct TmuxDetector;

#[async_trait]
impl HostDetector for TmuxDetector {
    fn name(&self) -> &str {
        "tmux"
    }

    async fn detect(&self, _runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion> {
        let mut assertions = Vec::new();

        if let Ok(value) = std::env::var("TMUX") {
            assertions.push(EnvironmentAssertion::EnvVarSet {
                key: "TMUX".into(),
                value,
            });
        }

        assertions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    #[tokio::test]
    async fn tmux_detector_inside_tmux() {
        unsafe {
            std::env::set_var("TMUX", "/tmp/tmux-1000/default,12345,0");
        }

        let runner = DiscoveryMockRunner::builder().build();
        let assertions = TmuxDetector.detect(&runner).await;

        unsafe {
            std::env::remove_var("TMUX");
        }

        assert_eq!(assertions.len(), 1);
        assert!(matches!(
            &assertions[0],
            EnvironmentAssertion::EnvVarSet { key, value }
            if key == "TMUX" && value == "/tmp/tmux-1000/default,12345,0"
        ));
    }

    #[tokio::test]
    async fn tmux_detector_not_inside() {
        unsafe {
            std::env::remove_var("TMUX");
        }

        let runner = DiscoveryMockRunner::builder().build();
        let assertions = TmuxDetector.detect(&runner).await;

        assert!(assertions.is_empty());
    }
}
