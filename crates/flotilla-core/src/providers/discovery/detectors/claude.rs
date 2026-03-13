//! Claude CLI host detector.

use std::path::Path;

use async_trait::async_trait;

use crate::providers::discovery::detectors::generic::parse_first_dotted_version;
use crate::providers::discovery::{EnvVars, EnvironmentAssertion, HostDetector};
use crate::providers::{run, CommandRunner};

/// Detects the `claude` CLI, checking PATH first, then known install locations.
pub struct ClaudeDetector;

#[async_trait]
impl HostDetector for ClaudeDetector {
    async fn detect(
        &self,
        runner: &dyn CommandRunner,
        _env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion> {
        // 1. Check PATH — single call proves existence and captures version
        if let Ok(output) = run!(runner, "claude", &["--version"], Path::new(".")) {
            return match parse_first_dotted_version(&output) {
                Some(version) => {
                    vec![EnvironmentAssertion::versioned_binary(
                        "claude", "claude", version,
                    )]
                }
                None => vec![EnvironmentAssertion::binary("claude", "claude")],
            };
        }

        // 2. Check known installation locations
        let known_paths = [dirs::home_dir().map(|h| h.join(".claude/local/claude"))];
        for path in known_paths.into_iter().flatten() {
            if path.is_file() {
                let path_str = path.to_str().unwrap_or("");
                if let Ok(output) = run!(runner, path_str, &["--version"], Path::new(".")) {
                    return match parse_first_dotted_version(&output) {
                        Some(version) => {
                            vec![EnvironmentAssertion::versioned_binary(
                                "claude", path, version,
                            )]
                        }
                        None => vec![EnvironmentAssertion::binary("claude", path)],
                    };
                }
            }
        }

        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars};
    use std::path::PathBuf;

    #[tokio::test]
    async fn claude_detector_found_on_path() {
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "claude",
                &["--version"],
                Ok("1.0.20 (Claude Code)\n".into()),
            )
            .build();
        let assertions = ClaudeDetector
            .detect(&runner, &TestEnvVars::default())
            .await;
        assert_eq!(assertions.len(), 1);
        match &assertions[0] {
            EnvironmentAssertion::BinaryAvailable {
                name,
                path,
                version,
            } => {
                assert_eq!(name, "claude");
                assert_eq!(path, &PathBuf::from("claude"));
                assert_eq!(version.as_deref(), Some("1.0.20"));
            }
            other => panic!("expected BinaryAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claude_detector_not_found() {
        // No on_run configured → run! returns Err for PATH check
        let runner = DiscoveryMockRunner::builder().build();
        let assertions = ClaudeDetector
            .detect(&runner, &TestEnvVars::default())
            .await;
        // May be empty or may find at known path — depends on filesystem.
        // At minimum, verify it doesn't panic and no PATH-based assertion is returned.
        for a in &assertions {
            match a {
                EnvironmentAssertion::BinaryAvailable { path, .. } => {
                    // If something was found, it shouldn't be the bare "claude" name
                    assert_ne!(path, &PathBuf::from("claude"));
                }
                _ => panic!("unexpected assertion type"),
            }
        }
    }
}
